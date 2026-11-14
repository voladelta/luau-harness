//! The processes hx starts and how they end. hx is a child subreaper (`init`), so every descendant that loses its
//! parent, including one that detached with `setsid` or a double fork, is re-parented to hx rather than to init. Each
//! long-lived child (a shell command, a local MCP server, an evaluator) starts in its own session and is registered
//! here (`GroupKill`) for as long as it runs. A process re-parented to hx whose session is neither hx's own nor a
//! registered child's is an orphan: something a finished command left behind. `sweep` kills and reaps those after a
//! shell command ends, and `shutdown` takes down everything hx started on the way out.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Make hx the reaper of its orphaned descendants. Call once at startup, before starting any child; the setting is
/// not inherited by children and survives exec.
pub fn init() -> std::io::Result<()> {
    // SAFETY: prctl(2) with PR_SET_CHILD_SUBREAPER takes only integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// What a registered child is: a shell command (or evaluator) ends on its own, a server lives until it is dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Command,
    Server,
}

/// The sessions of hx's running children, by session id (the leader's pid).
#[derive(Default)]
struct Registry {
    sessions: BTreeMap<i32, Kind>,
    /// Set by `shutdown`: nothing new starts once hx is on its way out.
    stopping: bool,
}

impl Registry {
    /// The sessions a sweep must spare, or None while a shell command still runs: a concurrent command may be using
    /// a daemon an earlier one started, so orphans are swept only when no command is running.
    fn spare(&self) -> Option<BTreeSet<i32>> {
        if self.sessions.values().any(|k| *k == Kind::Command) {
            return None;
        }
        Some(self.sessions.keys().copied().collect())
    }
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    sessions: BTreeMap::new(),
    stopping: false,
});

/// The registry, also after a panic elsewhere: it holds plain ids that stay consistent.
fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Child runs (autoresearch episodes), asked to stop when this process stops.
static CHILDREN: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

/// Make `command` start its child in a new session, so the child leads its own session and process group, and, when
/// `ruleset` is given, under that Landlock ruleset. The ruleset is applied last, after `setsid`. Start it with
/// `GroupKill::spawn`.
pub(crate) fn confine_command(
    command: &mut tokio::process::Command,
    ruleset: Option<Arc<crate::confine::Ruleset>>,
) {
    // SAFETY: the closure runs in the forked child before exec, where only async-signal-safe calls are allowed. It
    // calls setsid(2) and `Ruleset::apply` (prctl(2) and landlock_restrict_self(2)), and on failure reads errno into
    // an `io::Error`, which does not allocate. It takes no lock and does not allocate: the Arc was moved in by the
    // parent and is only borrowed here.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(ruleset) = &ruleset {
                ruleset.apply()?;
            }
            Ok(())
        });
    }
}

/// A registered child's session: while it lives, a sweep spares the session; when dropped (which also covers a
/// cancelled future) it kills the child's process group with SIGKILL and unregisters the session. Whatever survives
/// the group kill (a process that left the group) is an orphan from then on.
///
/// Contract: drop the guard before reaping the child or promptly after: once the leader is reaped and every group
/// member has exited, the kernel may reuse the group id.
pub(crate) struct GroupKill(i32);

impl GroupKill {
    /// Spawn `command` (prepared by `confine_command`) and register its session. Spawning and registering happen
    /// under the registry lock, so a sweep never sees the new leader unregistered.
    pub(crate) fn spawn(
        command: &mut tokio::process::Command,
        kind: Kind,
    ) -> std::io::Result<(tokio::process::Child, GroupKill)> {
        let mut registry = registry();
        if registry.stopping {
            return Err(std::io::Error::other("hx is stopping"));
        }
        let child = command.spawn()?;
        let session = i32::try_from(child.id().expect("a child that was just spawned has an id"))
            .expect("a pid fits in pid_t");
        registry.sessions.insert(session, kind);
        Ok((child, GroupKill(session)))
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        registry().sessions.remove(&self.0);
        // SAFETY: killpg(2) takes plain integers and has no memory-safety preconditions.
        unsafe { libc::killpg(self.0, libc::SIGKILL) };
    }
}

/// A child run, asked to stop (and so to stop its own commands) when this process stops.
pub struct Child(i32);

impl Child {
    pub fn track(pid: u32) -> Self {
        let pid = i32::try_from(pid).expect("a pid fits in pid_t");
        CHILDREN
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(pid);
        Child(pid)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        CHILDREN
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// After a shell command has ended and its group is killed: kill and reap the orphans, unless another shell command
/// is still running (then the last one to end does it). Blocking work, run off the async workers.
pub(crate) async fn sweep_after_command() -> std::io::Result<()> {
    tokio::task::spawn_blocking(|| {
        let registry = registry();
        match registry.spare() {
            Some(spare) => sweep(&spare),
            None => Ok(()),
        }
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Kill every running child's process group, ask every child run to stop, then kill and reap everything else hx's
/// children left behind. Nothing starts afterwards. Called on every way out of hx.
pub fn shutdown() {
    let mut registry = registry();
    registry.stopping = true;
    for session in std::mem::take(&mut registry.sessions).into_keys() {
        // SAFETY: killpg(2) takes plain integers and has no memory-safety preconditions.
        unsafe { libc::killpg(session, libc::SIGKILL) };
    }
    for child in CHILDREN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        // SAFETY: kill(2) takes plain integers and has no memory-safety preconditions.
        unsafe { libc::kill(*child, libc::SIGTERM) };
    }
    // The registry stays locked, so nothing is spawned between the scans.
    if let Err(e) = sweep(&BTreeSet::new()) {
        eprintln!("hx: cannot stop the processes its commands left behind: {e}");
    }
}

/// Kill everything this process started and exit (a signal, or the orchestrator of an episode is gone). Exiting here
/// runs no destructors, so it also ends the live policies' sessions (journal record, private temp directory).
pub fn stop(code: i32) -> ! {
    shutdown();
    crate::policy::end_live_sessions();
    std::process::exit(code)
}

/// Stop on SIGINT, SIGTERM or SIGHUP, taking every process this one started down with it.
pub fn stop_on_signals() {
    use tokio::signal::unix::{SignalKind, signal};
    let watch = |kind: SignalKind, code: i32| {
        if let Ok(mut s) = signal(kind) {
            tokio::spawn(async move {
                if s.recv().await.is_some() {
                    stop(code)
                }
            });
        }
    };
    watch(SignalKind::interrupt(), 130);
    watch(SignalKind::terminate(), 143);
    watch(SignalKind::hangup(), 129);
}

/// Kill and reap the orphans: hx's children outside hx's own session and outside `spare`. Repeats until a scan finds
/// none, because killing one re-parents its own children to hx. Reaps by pid only, never with `waitpid(-1)`: tokio
/// and std reap the children they started by pid and must not lose a status. Call with the registry locked.
fn sweep(spare: &BTreeSet<i32>) -> std::io::Result<()> {
    // SAFETY: getpid(2) and getsid(2) take plain integers and cannot fail for the calling process.
    let (me, own) = unsafe { (libc::getpid(), libc::getsid(0)) };
    // Children that cannot be killed (a setuid program) or reaped: never waited for, never retried.
    let mut stuck = BTreeSet::new();
    loop {
        let found: Vec<i32> = orphans(me, own, spare)?
            .into_iter()
            .filter(|pid| !stuck.contains(pid))
            .collect();
        if found.is_empty() {
            return Ok(());
        }
        let mut killed = Vec::with_capacity(found.len());
        for pid in found {
            // SAFETY: kill(2) takes plain integers. `pid` is an unreaped child of this process, so the id cannot
            // have been reused for another process.
            if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                killed.push(pid);
            } else {
                stuck.insert(pid);
            }
        }
        for pid in killed {
            loop {
                // SAFETY: waitpid(2) on one specific child; the status pointer is null, which the call permits.
                let r = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::__WALL) };
                if r == -1
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                if r == -1 {
                    stuck.insert(pid);
                }
                break;
            }
        }
    }
}

/// One scan: the pids whose parent is `me` and whose session is neither `own` nor in `spare`.
fn orphans(me: i32, own: i32, spare: &BTreeSet<i32>) -> std::io::Result<Vec<i32>> {
    let mut found = Vec::new();
    for pid in candidates(me)? {
        // A process that exited since it was listed has no stat any more; one hidden from hx is not hx's.
        let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some((parent, session)) = parent_and_session(&stat)
            && parent == me
            && session != own
            && !spare.contains(&session)
        {
            found.push(pid);
        }
    }
    Ok(found)
}

/// The processes that may be orphans of `me`. The kernel re-parents an orphan to the first live thread of its
/// subreaper, which is hx's main thread for as long as hx runs, so that thread's `children` list holds every orphan,
/// and reading it costs a few microseconds where a scan of every process in `/proc` costs about 15 microseconds per
/// process. A kernel without that file (`CONFIG_PROC_CHILDREN`) gets the full scan.
fn candidates(me: i32) -> std::io::Result<Vec<i32>> {
    match std::fs::read_to_string(format!("/proc/{me}/task/{me}/children")) {
        Ok(list) => Ok(list
            .split_ascii_whitespace()
            .filter_map(|pid| pid.parse().ok())
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => every_process(),
        Err(e) => Err(e),
    }
}

/// Every process in `/proc`.
fn every_process() -> std::io::Result<Vec<i32>> {
    let mut all = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        if let Some(pid) = entry?.file_name().to_str().and_then(|n| n.parse().ok()) {
            all.push(pid);
        }
    }
    Ok(all)
}

/// The parent pid and session id from a `/proc/PID/stat` line: `pid (comm) state ppid pgrp session ...`. The command
/// name may contain spaces and parentheses, so the fields are counted from the last `)`.
fn parent_and_session(stat: &[u8]) -> Option<(i32, i32)> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
    let mut fields = rest.split_ascii_whitespace();
    let _state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let _group = fields.next()?;
    let session = fields.next()?.parse().ok()?;
    Some((parent, session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_fields_are_counted_from_the_last_parenthesis() {
        assert_eq!(
            parent_and_session(b"42 (a) b) (c d) S 7 42 42 0 -1 4194560 0"),
            Some((7, 42))
        );
        assert_eq!(parent_and_session(b"42 (sh) R 1 2 3 0"), Some((1, 3)));
        assert_eq!(parent_and_session(b"42 (sh"), None);
        assert_eq!(parent_and_session(b"42 (sh) R x 2 3"), None);
    }

    #[test]
    fn the_full_scan_lists_every_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let me = i32::try_from(std::process::id()).unwrap();
        let all = every_process().unwrap();
        assert!(all.contains(&pid) && all.contains(&me), "{all:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn orphans_are_swept_only_when_no_command_runs() {
        let mut registry = Registry::default();
        registry.sessions.insert(10, Kind::Server);
        assert_eq!(registry.spare(), Some(BTreeSet::from([10])));
        registry.sessions.insert(11, Kind::Command);
        assert_eq!(registry.spare(), None);
    }

    /// Whether `pid` still exists (a zombie counts: it has not been reaped).
    fn exists(pid: i32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// The pid a script wrote to `file`, waiting for it: the script writes it once its escapee is in place.
    fn pid_in(file: &std::path::Path) -> i32 {
        let text = std::fs::read_to_string(file).unwrap();
        text.trim()
            .parse()
            .unwrap_or_else(|_| panic!("pid file {text:?}"))
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-procs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sweep_now() {
        let registry = registry();
        let spare = registry.sessions.keys().copied().collect();
        sweep(&spare).unwrap();
    }

    /// Each script leaves a process behind in a way that escapes the process-group kill, and writes its pid to `pid`
    /// only once it is in place (the reads of the fifo order this; no sleep guards a race). After the command and a
    /// sweep, that process is gone and reaped.
    #[tokio::test]
    async fn nothing_a_command_starts_outlives_it() {
        init().unwrap();
        let cases = [
            (
                "setsid",
                "setsid sh -c 'echo $$ > pid; echo > f; exec sleep 300' & read < f",
            ),
            (
                "double-fork",
                "python3 -c 'import os\nif os.fork(): os._exit(0)\nos.setsid()\nif os.fork(): os._exit(0)\n\
                 open(\"pid\", \"w\").write(str(os.getpid()))\nopen(\"f\", \"w\").write(\"x\\n\")\n\
                 os.execvp(\"sleep\", [\"sleep\", \"300\"])'; read < f",
            ),
            (
                "nohup",
                "nohup setsid sh -c 'echo $$ > pid; echo > f; exec sleep 300' >/dev/null 2>&1 & read < f",
            ),
            (
                "env-i-setsid",
                "env -i /usr/bin/setsid sh -c 'echo $$ > pid; echo > f; exec sleep 300' & read < f",
            ),
            (
                "renamed",
                "setsid bash -c 'printf \"x) S 1 1 1\" > /proc/self/comm; echo $$ > pid; echo > f; \
                 while :; do sleep 1; done' & read < f",
            ),
            (
                "ignores-term",
                "setsid sh -c 'trap \"\" TERM HUP INT; echo $$ > pid; echo > f; while :; do sleep 1; done' & read < f",
            ),
            (
                "grandchildren",
                "setsid sh -c 'setsid sh -c \"echo \\$\\$ > pid; echo > f; exec sleep 300\" & wait' & read < f",
            ),
        ];
        let env = crate::prims::ShellEnv {
            env: None,
            confine: None,
        };
        for (name, script) in cases {
            let dir = scratch(name);
            let script = format!("mkfifo f; {script}");
            let (_, se, code, timed_out, _) = crate::prims::shell(&env, &script, dir.clone(), 20.0)
                .await
                .unwrap();
            assert_eq!(
                (code, timed_out),
                (Some(0), false),
                "{name}: {}",
                String::from_utf8_lossy(&se)
            );
            let pid = pid_in(&dir.join("pid"));
            // Another test's command may be running, which defers the command's own sweep.
            tokio::task::spawn_blocking(sweep_now).await.unwrap();
            assert!(!exists(pid), "{name}: {pid} outlived its command");
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A registered server whose wrapper exited leaves its real server in the registered session: that survives a
    /// sweep, and dies with the server's group. A child in hx's own session is never touched, and keeps its status.
    #[tokio::test]
    async fn a_sweep_spares_registered_sessions_and_hx_session() {
        init().unwrap();
        let dir = scratch("server");
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "sleep 300 & echo $! > pid"])
            .current_dir(&dir)
            .kill_on_drop(true);
        confine_command(&mut command, None);
        let (mut wrapper, group) = GroupKill::spawn(&mut command, Kind::Server).unwrap();
        assert!(wrapper.wait().await.unwrap().success());
        let server = pid_in(&dir.join("pid"));
        let mut own = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        let own_pid = i32::try_from(own.id()).unwrap();
        // The child in hx's session has exited but is not reaped yet: wait until it is a zombie.
        let zombie = |stat: Vec<u8>| {
            let close = stat.iter().rposition(|&b| b == b')').unwrap();
            stat.get(close + 2) == Some(&b'Z')
        };
        while !std::fs::read(format!("/proc/{own_pid}/stat")).is_ok_and(zombie) {
            std::thread::yield_now();
        }
        tokio::task::spawn_blocking(sweep_now).await.unwrap();
        assert!(exists(server), "the registered server was killed");
        assert_eq!(own.wait().unwrap().code(), Some(7));
        drop(group);
        tokio::task::spawn_blocking(sweep_now).await.unwrap();
        assert!(!exists(server), "the server outlived its group");
        let _ = std::fs::remove_dir_all(dir);
    }
}
