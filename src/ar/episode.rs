//! Inside an episode: `ar.eval`, `ar.reset` and `ar.verdict`. A child run reads its episode once at launch from
//! `HX_AR_EPISODE` and records it in its frozen policy. It never touches the run directory: every `ar` call is a request
//! over the channel its orchestrator passed it (a socket on `FD`), which the orchestrator serves and journals.
use super::*;
use super::files::*;
use crate::policy::Policy;
use std::io::{BufRead, Write};
use std::os::fd::FromRawFd;

/// What a child run needs to serve `ar`; written by the orchestrator into `HX_AR_EPISODE`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    pub label: String,
    pub role: Role,
    /// The paths `ar.eval` captures and `ar.reset` restores.
    pub editable: Vec<String>,
    /// Paths the child's policy seals: this workspace's runs and the evaluators' roots, resolved by the orchestrator.
    pub sealed: Vec<PathBuf>,
    /// Where the child journals: in its copy beside its workspace, out of its shell's reach. The orchestrator keeps it
    /// with the run once the child ends.
    pub log: PathBuf,
}

/// The descriptor a child finds its channel on. A socket rather than stdin and stdout: stdout carries the child's
/// report, and a socket is one bidirectional descriptor whose end of input also says the orchestrator is gone.
pub(super) const FD: i32 = 3;

/// The channel to the orchestrator: a JSON line per request, `{id, op, ...}`, answered by `{id, ok, result | error}`.
struct Channel {
    out: std::sync::Mutex<std::os::unix::net::UnixStream>,
    waiting: std::sync::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>,
    next: std::sync::atomic::AtomicU64,
}
static CHANNEL: std::sync::OnceLock<Channel> = std::sync::OnceLock::new();

/// The episode this child `hx run` serves, if the orchestrator started it. Called once at launch, before anything is
/// spawned; the episode goes into the run's policy, which seals what it names and installs `ar`.
pub fn load_episode() -> Result<Option<Episode>> {
    let Some(text) = std::env::var_os("HX_AR_EPISODE") else { return Ok(None) };
    let episode: Episode = serde_json::from_slice(text.as_encoded_bytes()).context("HX_AR_EPISODE")?;
    // Close-on-exec first: the agent's commands must never hold the channel.
    let stream = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        ensure!(libc::fstat(FD, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK, "an autoresearch episode needs its orchestrator's channel");
        ensure!(libc::fcntl(FD, libc::F_SETFD, libc::FD_CLOEXEC) == 0, "{}", std::io::Error::last_os_error());
        std::os::unix::net::UnixStream::from_raw_fd(FD)
    };
    let input = stream.try_clone()?;
    let _ = CHANNEL.set(Channel { out: stream.into(), waiting: Default::default(), next: Default::default() });
    // The orchestrator holds the other end while it lives: end of input means it is gone, with any request it was
    // serving, so stop too.
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(input).lines() {
            let Ok(reply) = line.map(|l| serde_json::from_str::<Value>(&l).unwrap_or(Value::Null)) else { break };
            let waiting = reply["id"].as_u64().and_then(|id| CHANNEL.get()?.waiting.lock().unwrap().remove(&id));
            if let Some(tx) = waiting { let _ = tx.send(reply); }
        }
        crate::prims::stop(1);
    });
    Ok(Some(episode))
}

/// One request to the orchestrator, answered when it has done (and journaled) what was asked.
async fn request(mut req: Value) -> Result<Value> {
    let channel = CHANNEL.get().context("no channel to the orchestrator")?;
    let id = channel.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = tokio::sync::oneshot::channel();
    channel.waiting.lock().unwrap().insert(id, tx);
    req["id"] = json!(id);
    let line = serde_json::to_string(&req)? + "\n";
    // A snapshot can be megabytes: written off the runtime.
    tokio::task::spawn_blocking(move || CHANNEL.get().unwrap().out.lock().unwrap().write_all(line.as_bytes())).await??;
    let reply = rx.await.context("the orchestrator is gone")?;
    match reply["ok"].as_bool() {
        Some(true) => Ok(reply["result"].clone()),
        _ => bail!("{}", reply["error"].as_str().unwrap_or("the orchestrator refused the request")),
    }
}

fn served<'a>(pol: &'a Policy, what: &str) -> Result<&'a Episode> {
    pol.episode.as_ref().with_context(|| format!("{what} is only available inside an autoresearch episode"))
}

/// `ar.eval(note)`: snapshot the editable files and have the orchestrator score and record the state.
pub async fn eval(pol: &Policy, note: String) -> Result<Value> {
    let episode = served(pol, "ar.eval")?;
    let files = capture(&pol.workspace, &episode.editable)?;
    request(json!({"op": "eval", "note": note.chars().take(300).collect::<String>(), "files": to_wire(&files)})).await
}

/// `ar.reset()`: put this worker's best scored state back into the editable paths (the state it started from when it
/// has not improved on it).
pub async fn reset(pol: &Policy) -> Result<Value> {
    let episode = served(pol, "ar.reset")?;
    let reply = request(json!({"op": "reset"})).await?;
    let files = from_wire(&episode.editable, &reply["files"])?;
    // The agent controls this workspace, so every change goes the way its own `write` would: a policy-resolved path,
    // and directories opened without following links.
    place_checked(&std::fs::canonicalize(&pol.workspace)?, &episode.editable, &files, &|rel| pol.check_write(rel))?;
    Ok(json!({"best": reply["best"]}))
}

/// `ar.verdict{pass, reason}`: an auditor's verdict on its candidate, final once given. A fail needs its reason. A typed
/// call rather than a line of prose, so an auditor stopped by its cap after deciding still counts.
pub async fn submit_verdict(pol: &Policy, pass: bool, reason: Option<String>) -> Result<Value> {
    ensure!(served(pol, "ar.verdict")?.role == Role::Audit, "ar.verdict is for auditors");
    let reason = reason.unwrap_or_default();
    ensure!(pass || !reason.trim().is_empty(), "a failing verdict needs its reason: ar.verdict{{pass = false, reason = \"...\"}}");
    request(json!({"op": "verdict", "pass": pass, "reason": reason})).await
}
