//! Autoresearch (`hx run --ar`): a manager loop that improves a score by changing a task's editable files, and returns
//! a portfolio of distinct good candidates rather than one best state.
//!
//! The task supplies `.harness/ar.toml` (editable paths, evaluator commands, grader and holdout roots) and the program
//! text (`--prompt-file`). At launch the committed tree is frozen into the run's `base`. Agents never touch the owner's
//! workspace or its Git repository:
//!
//! - Every episode is a fresh child `hx run` in a throwaway copy of `base` holding the state it starts from. Only
//!   snapshots of the editable paths ever leave an episode.
//! - `ar.eval` snapshots the editable files and scores the snapshot in a second throwaway copy of `base`, so the
//!   evaluator sees exactly the recorded bytes and nothing else the agent did.
//! - Evaluators run under their own sandbox: a scrubbed environment, no network, reads limited to the system,
//!   toolchains, the evaluation copy and the declared grader (and, for `select`, holdout) roots.
//! - The run directory (`.harness/ar/RUN`, with `base`, snapshots, ledger, claims, journal and held-out scores) is
//!   sealed from agents in every mode.
//!
//! A run is research, then rounds. A research episode studies the task and writes the brief every worker gets. Round 1
//! runs explore workers side by side; each derives its own idea from a host seed and claims it with `ar.claim`. Claims
//! are served one at a time in arrival order: an idea that two judge calls both find to share the core approach of an
//! earlier claim is rejected, and the worker tries another. Each accepted idea is a candidate with its own best state. A
//! gate hands the three strongest by search (good ones first, but an unpolished idea below the seed too) to the
//! exploit rounds, whose workers claim distinct refinements of their candidate. Good means beating the seed on search
//! and not being worse on the held-out `select`; good candidates are audited, and only they can become results. The
//! held-out score is only ever a veto: ranking on it as well would select on it every round until it is as optimistic
//! as the search score.
//!
//! Rounds go on while the budget lasts: after an exploit round that moves no chosen candidate, the next round explores
//! new ideas again, and when that moves none either the run stops. With no valid candidate after round 1 the run stops.
//! The result is the audited good candidates of the last gate, best search score first: several distinct good answers,
//! since the single best score is the one most likely to be an overfit or an evaluator hole.
use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Workers side by side in a round, and fewer once the provider has rate-limited an episode of the run.
const WORKERS: usize = 8;
const NARROW: usize = 6;
/// Rounds an episode's share is sized for: each gets at most budget / (1 + PACE * (WORKERS + TOP)). Rounds go on
/// while the budget lasts, so episodes that spend less than their share buy more rounds.
const PACE: usize = 3;
/// Candidates a gate hands to the next round, and the size of the final portfolio.
const TOP: usize = 3;
/// Ledger rows shown to a worker, characters of an evaluator report, of a brief and of one finding.
const SHOWN: usize = 40;
/// Findings shown to a worker: the most recent ones.
const FINDINGS: usize = 2 * WORKERS;
const REPORT_CHARS: usize = 12_000;
const BRIEF_CHARS: usize = 8_000;
const FINDING_CHARS: usize = 1_500;
/// Attempts of a planned episode that crashes before any eval (usually provider rate limits).
const MAX_CRASHES: u32 = 3;
/// Limits on the editable files of one snapshot: total bytes, file count and directory depth.
const MAX_SNAPSHOT: usize = 16 << 20;
const MAX_FILES: usize = 10_000;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Files or directories the agent may change; only these are carried between episodes and scored.
    pub editable: Vec<String>,
    /// Search evaluator: agent-visible. Prints a JSON object with `score` (and optionally `valid`, `se`, `report`)
    /// as its last stdout line.
    pub eval: String,
    /// Selection evaluator (held-out), never shown to agents: a veto, so a candidate worse than the seed on it never
    /// becomes a result.
    pub select: Option<String>,
    /// Directories both evaluators may read (the grader and its search data), outside the workspace. `~/` and
    /// `$HOME/` expand.
    #[serde(default)]
    pub grader: Vec<String>,
    /// Directories only the select evaluator may read (held-out data), so code under test in a search evaluation
    /// can never see them.
    #[serde(default)]
    pub holdout: Vec<String>,
    #[serde(default = "Spec::max")]
    pub direction: String,
    #[serde(default = "Spec::timeout")]
    pub timeout: f64,
    /// An eval improves on the best only by more than this (and more than the evaluator's reported `se`).
    #[serde(default)]
    pub min_delta: f64,
}
impl Spec {
    fn max() -> String { "max".into() }
    fn timeout() -> f64 { 600.0 }
    pub fn load(workspace: &Path) -> Result<Spec> {
        let path = workspace.join(".harness/ar.toml");
        let text = std::fs::read_to_string(&path).with_context(|| format!("--ar needs {}", path.display()))?;
        let spec: Spec = toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        ensure!(!spec.editable.is_empty(), "ar.toml: editable must name at least one path");
        for p in &spec.editable {
            let parts: Vec<&str> = p.trim_end_matches('/').split('/').collect();
            ensure!(!p.is_empty() && !Path::new(p).is_absolute()
                && parts.iter().all(|c| !c.is_empty() && *c != "." && *c != "..")
                && !matches!(parts[0], ".harness" | ".git"),
                "ar.toml: editable path {p:?} must be a relative path inside the workspace, outside .harness and .git");
        }
        for g in spec.roots(true)? {
            ensure!(!g.starts_with(workspace) && !workspace.starts_with(&g),
                "ar.toml: grader and holdout root {} must not overlap the workspace", g.display());
        }
        ensure!(matches!(spec.direction.as_str(), "max" | "min"), "ar.toml: direction must be max or min");
        ensure!(spec.timeout > 0.0 && spec.min_delta >= 0.0, "ar.toml: timeout must be positive and min_delta nonnegative");
        Ok(spec)
    }
    /// Read roots of an evaluator: `grader`, plus `holdout` for the select evaluator.
    fn roots(&self, holdout: bool) -> Result<Vec<PathBuf>> {
        let home = std::env::var("HOME").unwrap_or_default();
        let extra = if holdout { &self.holdout[..] } else { &[] };
        self.grader.iter().chain(extra).map(|g| {
            let expanded = g.strip_prefix("~/").or_else(|| g.strip_prefix("$HOME/"))
                .map(|rest| format!("{home}/{rest}")).unwrap_or_else(|| g.clone());
            std::fs::canonicalize(&expanded).with_context(|| format!("ar.toml: grader root {g}"))
        }).collect()
    }
    /// Is `a` better than `b` by more than `margin`?
    fn better(&self, a: f64, b: Option<f64>, margin: f64) -> bool {
        match b {
            None => true,
            Some(b) if self.direction == "max" => a > b + margin,
            Some(b) => a < b - margin,
        }
    }
}

/// What an episode is for. Research writes the brief; explore workers claim and test a new idea; exploit workers claim
/// and test a refinement of one candidate; auditors judge whether a candidate's score is real. Research and audit
/// evaluations are probes: recorded, never a candidate's state.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role { Research, Explore, Exploit, Audit }

/// What a child run needs to serve `ar`; written by the orchestrator, named by `HX_AR_EPISODE`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    /// The run directory; sealed from the child's agent.
    pub dir: PathBuf,
    pub label: String,
    pub spec: Spec,
    pub role: Role,
    /// The candidate an exploit worker improves (0 for every other role).
    pub idea: u32,
    /// The snapshot the workspace starts from, and the score an improvement must beat (none: any valid score).
    pub start: String,
    pub best: Option<f64>,
}

/// A worker's progress inside its child run: its claim and its best score so far.
pub struct Progress {
    claim: Option<u32>,
    best: Option<f64>,
}
pub static EPISODE: OnceLock<(Episode, Mutex<Progress>)> = OnceLock::new();

/// Called once at startup: a child run of an autoresearch episode gets `ar`, and its policy seals the run.
pub fn load_episode() -> Result<()> {
    if let Some(path) = std::env::var_os("HX_AR_EPISODE") {
        let episode: Episode = serde_json::from_slice(&std::fs::read(&path)?).context("HX_AR_EPISODE")?;
        let best = episode.best;
        let _ = EPISODE.set((episode, Mutex::new(Progress { claim: None, best })));
    }
    Ok(())
}

/// Paths a child's policy must seal, so that even with --yolo its agent cannot read or change them: every
/// autoresearch run of the owner's workspace (this run included) and the evaluators' roots.
pub fn sealed() -> Vec<PathBuf> {
    let Some((e, _)) = EPISODE.get() else { return vec![] };
    let mut out = vec![e.dir.parent().unwrap_or(&e.dir).to_path_buf()];
    out.extend(e.spec.roots(true).unwrap_or_default());
    out
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Row {
    n: usize,
    episode: String,
    /// The candidate this state belongs to; 0 for research and audit probes.
    #[serde(default)]
    idea: u32,
    note: String,
    snap: String,
    score: Option<f64>,
    valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se: Option<f64>,
    improved: bool,
    /// The search evaluator's report, shown to later episodes for a candidate's best state.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    report: Value,
}

/// One accepted claim. Explore claims share the scope `explore` and their number is the candidate's id; an exploit
/// worker's refinement claims are scoped to its candidate (`c3`).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Claim {
    scope: String,
    n: u32,
    idea: String,
    worker: String,
}

fn ledger(dir: &Path) -> Result<Vec<Row>> {
    read_lines(&dir.join("ledger.jsonl"))
}

/// Records of an append-only JSONL file. A torn final line (a writer killed mid-append) is ignored; damage anywhere
/// else is an error.
fn read_lines<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(vec![]) };
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    complete.lines().filter(|l| !l.trim().is_empty()).enumerate()
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("{} line {}", path.display(), i + 1)))
        .collect()
}

/// Append one record durably, first dropping a torn final line so the file stays parseable.
fn append_line<T: Serialize>(path: &Path, record: &T) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).read(true).append(true).open(path)?;
    let text = std::fs::read(path)?;
    if !text.is_empty() && !text.ends_with(b"\n") {
        file.set_len(text.iter().rposition(|b| *b == b'\n').map_or(0, |i| i as u64 + 1))?;
    }
    writeln!(file, "{}", serde_json::to_string(record)?)?;
    file.sync_all()?;
    Ok(())
}

/// Editable files, path -> bytes.
type Files = BTreeMap<String, Vec<u8>>;

/// Every regular file under the editable paths of `root`, within the snapshot limits. Symlinks and special files are
/// skipped: they never leave an episode. Names must be UTF-8 so they are recreated exactly.
fn capture(root: &Path, spec: &Spec) -> Result<Files> {
    fn walk(path: &Path, rel: &str, depth: usize, out: &mut Files, total: &mut usize) -> Result<()> {
        let Ok(meta) = std::fs::symlink_metadata(path) else { return Ok(()) };
        if meta.is_dir() {
            ensure!(depth < MAX_DEPTH, "editable directories nest deeper than {MAX_DEPTH} levels");
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name().into_string().map_err(|n| anyhow::anyhow!("editable file name {n:?} is not UTF-8"))?;
                walk(&entry.path(), &format!("{rel}/{name}"), depth + 1, out, total)?;
            }
        } else if meta.is_file() {
            ensure!(out.len() < MAX_FILES, "editable paths hold more than {MAX_FILES} files");
            // Read at most what is left of the limit, so a file that keeps growing cannot get past it.
            let left = MAX_SNAPSHOT - *total;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(std::fs::File::open(path)?, left as u64 + 1), &mut bytes)?;
            ensure!(bytes.len() <= left, "editable files exceed {} MiB", MAX_SNAPSHOT >> 20);
            *total += bytes.len();
            out.insert(rel.to_string(), bytes);
        }
        Ok(())
    }
    let (mut out, mut total) = (BTreeMap::new(), 0);
    for e in &spec.editable {
        let e = e.trim_end_matches('/');
        // A symlinked parent directory is skipped like any symlink: nothing is read through it.
        let linked = Path::new(e).ancestors().skip(1).filter(|a| !a.as_os_str().is_empty())
            .any(|a| std::fs::symlink_metadata(root.join(a)).is_ok_and(|m| m.file_type().is_symlink()));
        if linked { continue; }
        walk(&root.join(e), e, 0, &mut out, &mut total)?;
    }
    Ok(out)
}

/// Kill every process that still has its working directory or an open file under `dir` (agents' and evaluators'
/// detached leftovers), before the directory is deleted.
fn reap(dir: &Path) {
    let Ok(out) = std::process::Command::new("lsof").args(["-t", "+D"]).arg(dir).stderr(std::process::Stdio::null()).output() else { return };
    let me = std::process::id() as i32;
    for pid in String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse::<i32>().ok()).filter(|p| *p != me) {
        unsafe { libc::kill(pid, libc::SIGKILL); }
    }
}

/// Make the editable paths of `root` hold exactly `files`.
fn place(root: &Path, spec: &Spec, files: &Files) -> Result<()> {
    for e in &spec.editable {
        let path = root.join(e.trim_end_matches('/'));
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path)?,
            Ok(_) => std::fs::remove_file(&path)?,
            Err(_) => {}
        }
    }
    for (rel, bytes) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

/// Store a snapshot content-addressed under `dir/snaps`; returns its id.
fn save_snap(dir: &Path, files: &Files) -> Result<String> {
    let encoded: BTreeMap<&String, String> = files.iter()
        .map(|(k, v)| (k, base64::engine::general_purpose::STANDARD.encode(v))).collect();
    let bytes = serde_json::to_vec(&encoded)?;
    let id = crate::eventlog::sha_bytes(&bytes)[..16].to_string();
    let path = dir.join("snaps").join(format!("{id}.json"));
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, bytes)?;
    }
    Ok(id)
}

fn load_snap(dir: &Path, id: &str) -> Result<Files> {
    let encoded: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(dir.join("snaps").join(format!("{id}.json")))?)?;
    encoded.into_iter().map(|(k, v)| Ok((k, base64::engine::general_purpose::STANDARD.decode(v)?))).collect()
}

/// A fresh private directory in the system temp dir; removed on drop.
struct Scratch(PathBuf);
impl Scratch {
    fn new(kind: &str) -> Result<Scratch> {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::fs::canonicalize(std::env::temp_dir())?.join(format!("hx-ar-{kind}-{}-{nanos}-{seq}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Scratch(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        reap(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copy the tree `from` to the new path `to` (APFS clones when available).
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for flags in [&["-cR"][..], &["-R"][..]] {
        let status = std::process::Command::new("cp").args(flags).arg(from).arg(to)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()?;
        if status.success() { return Ok(()); }
        let _ = std::fs::remove_dir_all(to);
    }
    bail!("could not copy {} to {}", from.display(), to.display())
}

/// Run an evaluator on `files` placed into a fresh copy of the frozen base. Returns (score, valid, se, report).
/// Failures of the evaluator become invalid results, not errors.
async fn evaluate(spec: &Spec, run: &Path, files: &Files, cmd: &str, holdout: bool) -> Result<(Option<f64>, bool, Option<f64>, Value)> {
    let scratch = Scratch::new("eval")?;
    let tree = scratch.0.join("tree");
    copy_tree(&run.join("base"), &tree)?;
    place(&tree, spec, files)?;
    let tmp = scratch.0.join("tmp");
    std::fs::create_dir(&tmp)?;
    let mut read = spec.roots(holdout)?;
    read.push(scratch.0.clone());
    let jail = crate::prims::Jail {
        profile: crate::policy::evaluator_profile(&read, std::slice::from_ref(&scratch.0)),
        env: Some(evaluator_env(&tmp)),
    };
    let (stdout, stderr, code, timed_out) = crate::prims::shell(&jail, cmd, tree, spec.timeout).await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let stdout = String::from_utf8_lossy(&stdout);
    let tail = |s: &str| s.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect::<String>();
    let parsed = stdout.lines().rev().find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<Value>(l).ok()).filter(Value::is_object);
    let Some(obj) = parsed.filter(|_| code == Some(0) && !timed_out) else {
        let why = if timed_out { "evaluator timed out".to_string() } else { format!("evaluator exit {code:?} or no JSON result line") };
        return Ok((None, false, None, json!({"error": why, "stdout": tail(&stdout), "stderr": tail(&String::from_utf8_lossy(&stderr))})));
    };
    let score = obj["score"].as_f64().filter(|s| s.is_finite());
    let valid = score.is_some() && obj["valid"].as_bool() != Some(false);
    let se = obj["se"].as_f64().filter(|s| s.is_finite() && *s >= 0.0);
    Ok((score, valid, se, obj.get("report").cloned().unwrap_or(Value::Null)))
}

/// The evaluator's environment: no credentials, nothing inherited beyond what locates tools.
fn evaluator_env(tmp: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ["PATH", "HOME", "LANG", "LC_ALL", "USER", "RUSTUP_HOME", "CARGO_HOME"].iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect();
    env.push(("TMPDIR".into(), tmp.to_string_lossy().to_string()));
    env
}

/// `ar.eval(note)` inside an episode: snapshot the editable files, score the snapshot, append to the ledger. Explore and
/// exploit workers score only after their claim is accepted, and their states belong to their candidate.
pub async fn eval(pol: &crate::policy::Policy, note: String) -> Result<Value> {
    let (episode, progress) = EPISODE.get().context("ar.eval is only available inside an autoresearch episode")?;
    let spec = &episode.spec;
    let claim = progress.lock().unwrap().claim;
    let idea = match (episode.role, claim) {
        (Role::Explore | Role::Exploit, None) => bail!("claim your idea with ar.claim(\"...\") before scoring it"),
        (Role::Explore, Some(n)) => n,
        (Role::Exploit, Some(_)) => episode.idea,
        (Role::Research | Role::Audit, _) => 0,
    };
    let files = capture(&pol.workspace, spec)?;
    let snap = save_snap(&episode.dir, &files)?;
    // An evaluator that reports no `se` is deterministic: a state it already scored validly keeps its stored result. A
    // probe, or a state its candidate already has, gets that result and no new ledger row; a state only another
    // candidate scored is recorded for this one, judged against its best, without running the evaluator again.
    let rows = ledger(&episode.dir)?;
    let stored = rows.iter().find(|r| r.snap == snap && r.valid && r.se.is_none());
    if let Some(r) = stored.filter(|_| idea == 0 || rows.iter().any(|o| o.snap == snap && o.idea == idea && o.valid && o.se.is_none())) {
        let best = progress.lock().unwrap().best;
        return Ok(json!({"n": r.n, "score": r.score, "valid": true, "se": null, "improved": false, "best": best,
            "report": r.report, "cached": true}));
    }
    let (score, valid, se, report) = match stored {
        Some(r) => (r.score, true, None, r.report.clone()),
        None => evaluate(spec, &episode.dir, &files, &spec.eval, false).await?,
    };
    // A worker's step counts when it beats its best by more than the evaluator's noise, so small steps add up across
    // rounds; `min_delta` is the bar against the seed, applied at the gate.
    let margin = se.unwrap_or(0.0);
    let (improved, best) = {
        let mut p = progress.lock().unwrap();
        // The state a worker starts from never improves it, so an explore worker's bar is set by the first state it builds.
        let improved = idea != 0 && valid && snap != episode.start
            && spec.better(score.unwrap(), p.best, if p.best.is_some() { margin } else { 0.0 });
        if improved { p.best = score; }
        (improved, p.best)
    };
    let n = {
        let _lock = lock_file(&episode.dir.join("ledger.lock"))?;
        let n = ledger(&episode.dir)?.len() + 1;
        append_line(&episode.dir.join("ledger.jsonl"), &Row { n, episode: episode.label.clone(), idea,
            note: note.chars().take(300).collect(), snap, score, valid, se, improved, report: report.clone() })?;
        n
    };
    let mut out = json!({"n": n, "score": score, "valid": valid, "se": se, "improved": improved, "best": best, "report": report});
    if stored.is_some() { out["cached"] = json!(true); }
    Ok(out)
}

/// `ar.claim(idea)`: claim the idea an explore worker will test, or the refinement an exploit worker will try. Claims
/// are served one at a time in arrival order under the run's claim lock. A claim is rejected only when two independent
/// judge calls both find that it shares the core approach of the same earlier claim in its scope, so a borderline idea
/// gets the benefit of the doubt; a judge that cannot answer accepts. One accepted claim per worker.
pub async fn claim(host: &crate::host::Host, idea: String) -> Result<Value> {
    let (episode, progress) = EPISODE.get().context("ar.claim is only available inside an autoresearch episode")?;
    let scope = match episode.role {
        Role::Explore => "explore".to_string(),
        Role::Exploit => format!("c{}", episode.idea),
        Role::Research | Role::Audit => bail!("ar.claim is for explore and exploit workers"),
    };
    let idea: String = idea.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(1_000).collect();
    // Agents probe a function with a dummy argument ("x"); an accepted claim is final, so a non-description is refused
    // without using up the claim.
    ensure!(idea.split(' ').count() >= 8, "describe the idea in two or three sentences (at least 8 words): an accepted \
claim is final and is how other workers, auditors and the report know what you built");
    let path = episode.dir.join("claims.lock");
    let lock = tokio::task::spawn_blocking(move || lock_file(&path)).await??;
    if let Some(n) = progress.lock().unwrap().claim { bail!("this worker already claimed #{n}: one idea per worker") }
    let earlier: Vec<Claim> = read_lines::<Claim>(&episode.dir.join("claims.jsonl"))?.into_iter().filter(|c| c.scope == scope).collect();
    // A retried episode (label `r1-w3.2`) continues the claim an earlier attempt of the same plan made.
    let plan = |label: &str| label.split('.').next().unwrap_or(label).to_string();
    if let Some(mine) = earlier.iter().find(|c| plan(&c.worker) == plan(&episode.label)) {
        progress.lock().unwrap().claim = Some(mine.n);
        return Ok(json!({"ok": true, "n": mine.n, "idea": mine.idea, "message": format!("Accepted as #{}: an earlier attempt \
of this worker claimed this idea before it was interrupted. Continue with it: {}", mine.n, mine.idea)}));
    }
    // Plain text, so a printed result shows every claim (printers abbreviate nested tables).
    let listed: String = earlier.iter().map(|c| format!("{}. {}\n", c.n, c.idea)).collect();
    let what = if episode.role == Role::Exploit { "refinement" } else { "core approach" };
    match judge(host, &earlier, &idea, episode.role == Role::Exploit).await {
        Judged::New => {},
        Judged::Same(i) => return Ok(json!({"ok": false, "same_as": earlier[i].n, "message": format!("Rejected: the same \
{what} as #{} was claimed first. Claimed so far:\n{listed}Choose a different {what} and claim it.", earlier[i].n)})),
        Judged::Placeholder => return Ok(json!({"ok": false, "message": format!("Rejected: this names no concrete \
{what}. A claim is final and is how everyone knows what you built: claim the {what} you will build. `ar.claims()` lists \
what is taken. Claimed so far:\n{listed}")})),
    }
    let n = earlier.len() as u32 + 1;
    append_line(&episode.dir.join("claims.jsonl"), &Claim { scope, n, idea, worker: episode.label.clone() })?;
    // Recorded before the lock is released, so a second claim of this worker waiting on the lock sees it.
    progress.lock().unwrap().claim = Some(n);
    drop(lock);
    Ok(json!({"ok": true, "n": n, "message": format!("Accepted as #{n}. Build it now; ar.eval works from here on.")}))
}

/// What two independent judge calls agree a new claim is.
enum Judged {
    New,
    /// The index of the earlier claim it duplicates: the same core approach for explore ideas, the same change for
    /// refinements of one candidate.
    Same(usize),
    /// No concrete approach at all: a placeholder, a probe or a test of the tool.
    Placeholder,
}

/// Judge a new claim against the earlier ones. Only an answer both calls agree on rejects it; anything else accepts.
async fn judge(host: &crate::host::Host, earlier: &[Claim], idea: &str, refinement: bool) -> Judged {
    let listed: String = if earlier.is_empty() { "(none)\n".into() } else { earlier.iter().map(|c| format!("{}. {}\n", c.n, c.idea)).collect() };
    let prompt = if refinement {
        format!("Several researchers each claim one refinement of the same solution. Claimed so far:\n{listed}\n\
New claim:\n{idea}\n\nIs the new claim the same refinement as one already claimed? Same means the same change to the \
same part of the solution (the same rule, component or parameter, changed the same way); a change to a different part, \
or a different kind of change, is a different refinement even when both touch similar things. In one sentence, name \
what the new claim changes and the closest claimed one. Then end with a line 'Answer: N', where N is the matching \
claim's number, or 0 if it is a different refinement. If the new claim names no concrete change at all (a placeholder, \
a probe or a test of the tool), end with 'Answer: none' instead.")
    } else {
        format!("Several researchers each claim one core approach to the same task. Claimed so far:\n{listed}\n\
New claim:\n{idea}\n\nDoes the new claim use the same core approach as one already claimed? Same core means the same main \
mechanism that decides what the solution does (the same kind of model, algorithm or signal); differing only in \
parameters, filters, exits, sizing or added gates still counts as the same, while a different main mechanism is a \
different core even when it shares ingredients. In one sentence, name the new claim's main mechanism and the closest \
claimed one. Then end with a line 'Answer: N', where N is the matching claim's number, or 0 if it is a genuinely \
different core approach. If the new claim names no concrete approach at all (a placeholder, a probe or a test of the \
tool), end with 'Answer: none' instead.")
    };
    let request = json!({"system": "You compare research ideas and answer exactly in the format asked.", "prompt": prompt,
        "effort": "none", "max_tokens": 2000});
    let Ok(replies) = host.complete(json!([request, request])).await else { return Judged::New };
    let picks: Vec<Option<usize>> = replies.as_array().into_iter().flatten().map(|r| r["value"].as_str().and_then(answer_number)).collect();
    match picks.as_slice() {
        [Some(0), Some(0)] => Judged::New,
        [Some(a), Some(b)] if a == b && *a == usize::MAX => Judged::Placeholder,
        [Some(a), Some(b)] if a == b && (1..=earlier.len()).contains(a) => Judged::Same(a - 1),
        _ => Judged::New,
    }
}

/// N from the last `Answer: N` in a judge's reply, tolerating markdown such as `**Answer:** #2`; `Answer: none` is
/// `usize::MAX`.
fn answer_number(text: &str) -> Option<usize> {
    let at = text.to_ascii_lowercase().rfind("answer")?;
    let rest = text[at + "answer".len()..].trim_start_matches(['*', ':', ' ', '#', '\t']);
    if rest.to_ascii_lowercase().starts_with("none") { return Some(usize::MAX); }
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// `ar.claims()`: the claims made so far in this worker's scope, as plain text.
pub fn claims() -> Result<Value> {
    let (episode, _) = EPISODE.get().context("ar.claims is only available inside an autoresearch episode")?;
    let scope = match episode.role {
        Role::Explore => "explore".to_string(),
        Role::Exploit => format!("c{}", episode.idea),
        Role::Research | Role::Audit => bail!("ar.claims is for explore and exploit workers"),
    };
    let listed: String = read_lines::<Claim>(&episode.dir.join("claims.jsonl"))?.into_iter().filter(|c| c.scope == scope)
        .map(|c| format!("{}. {}\n", c.n, c.idea)).collect();
    Ok(json!({"message": if listed.is_empty() { "No claims yet.".to_string() } else { format!("Claimed so far:\n{listed}") }}))
}

/// An exclusive lock on `path`, held until the returned file is dropped: episodes running side by side number ledger
/// rows without gaps or repeats, and claims are judged one at a time in arrival order.
fn lock_file(path: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    ensure!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0, "could not lock {}", path.display());
    Ok(file)
}

/// `ar.reset()`: put this worker's best scored state back into the editable paths (the state it started from when it
/// has not improved on it).
pub fn reset(pol: &crate::policy::Policy) -> Result<Value> {
    let (episode, progress) = EPISODE.get().context("ar.reset is only available inside an autoresearch episode")?;
    let rows = ledger(&episode.dir)?;
    let snap = rows.iter().rev().find(|r| r.improved && r.episode == episode.label)
        .map_or_else(|| episode.start.clone(), |r| r.snap.clone());
    let files = load_snap(&episode.dir, &snap)?;
    // The agent controls this workspace, so every change goes the way its own `write` would: a policy-resolved path,
    // and directories opened without following links.
    let target = |rel: &str, create| -> Result<crate::atomic_file::Target> {
        Ok(crate::atomic_file::Target::open(&pol.check_write(rel)?, create)?)
    };
    for rel in capture(&pol.workspace, &episode.spec)?.keys().filter(|rel| !files.contains_key(*rel)) {
        target(rel, false)?.remove()?;
    }
    for (rel, bytes) in &files {
        target(rel, true)?.replace(bytes)?;
    }
    Ok(json!({"best": progress.lock().unwrap().best}))
}

/// Git, configured so a repository's own config cannot run commands (owner context, launch only).
fn git(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "core.untrackedCache=false"])
        .args(args).current_dir(workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output().context("git")?;
    ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

/// Paths that differ from HEAD outside the editable set (owner's `.harness/` excepted).
fn uncommitted(workspace: &Path, spec: &Spec) -> Result<Vec<String>> {
    let out = git(workspace, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let mut bad = vec![];
    let mut entries = out.split(|b| *b == 0).filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let entry = String::from_utf8_lossy(entry);
        let (status, path) = entry.split_at(3.min(entry.len()));
        let mut paths = vec![path.to_string()];
        // A rename or copy names its source as the next entry; both must be in scope.
        if status.starts_with('R') || status.starts_with('C') {
            paths.extend(entries.next().map(|e| String::from_utf8_lossy(e).to_string()));
        }
        for p in paths {
            let inside = spec.editable.iter().any(|e| p == *e || p.starts_with(&format!("{}/", e.trim_end_matches('/'))));
            if !p.starts_with(".harness/") && !inside { bad.push(p); }
        }
    }
    Ok(bad)
}

/// Freeze HEAD's tree (without `.harness`) into `run/base`, byte for byte: blobs are read directly, so export
/// attributes (`export-ignore`, `export-subst`) cannot change what the evaluator sees. Submodules are not included.
fn freeze(workspace: &Path, run: &Path) -> Result<String> {
    use std::io::{BufRead, Read, Write};
    let commit = String::from_utf8(git(workspace, &["rev-parse", "HEAD"])?)?.trim().to_string();
    let listing = git(workspace, &["ls-tree", "-r", "-z", "--full-tree", &commit])?;
    let entries: Vec<(String, String, String)> = listing.split(|b| *b == 0).filter(|e| !e.is_empty()).map(|e| {
        let e = String::from_utf8(e.to_vec()).map_err(|_| anyhow::anyhow!("non-UTF-8 path in the committed tree"))?;
        let (meta, path) = e.split_once('\t').context("ls-tree entry")?;
        let mut parts = meta.split(' ');
        let (mode, kind, sha) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        Ok((mode.to_string(), if kind == "blob" { sha.to_string() } else { String::new() }, path.to_string()))
    }).collect::<Result<_>>()?;
    let base = run.join("base");
    std::fs::create_dir_all(&base)?;
    let mut child = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "cat-file", "--batch"])
        .current_dir(workspace).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn()?;
    let mut input = child.stdin.take().unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    for (mode, sha, path) in entries.iter().filter(|(_, sha, _)| !sha.is_empty()) {
        if path == ".harness" || path.starts_with(".harness/") {
            continue;
        }
        ensure!(!path.split('/').any(|c| c == ".." || c.is_empty()), "unsafe path {path:?} in the committed tree");
        writeln!(input, "{sha}")?;
        input.flush()?;
        let mut header = String::new();
        output.read_line(&mut header)?;
        let size: usize = header.trim().rsplit(' ').next().and_then(|n| n.parse().ok()).context("cat-file header")?;
        let mut bytes = vec![0; size + 1];
        output.read_exact(&mut bytes)?;
        bytes.pop();
        let target = base.join(path);
        if let Some(parent) = target.parent() { std::fs::create_dir_all(parent)?; }
        if mode == "120000" {
            std::os::unix::fs::symlink(String::from_utf8_lossy(&bytes).to_string(), &target)?;
        } else {
            std::fs::write(&target, &bytes)?;
            if mode == "100755" {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            }
        }
    }
    drop(input);
    ensure!(child.wait()?.success(), "git cat-file failed");
    Ok(commit)
}

/// Launch settings, frozen at the start and reused on resume.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Launch {
    spec: Spec,
    program: String,
    child_args: Vec<String>,
    budget: f64,
    commit: String,
}

/// One planned episode. A batch (the research, a round's workers, a gate's audits) is planned whole before any of it
/// starts, so a resumed run finishes exactly the batch it was in.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Plan {
    label: String,
    role: Role,
    idea: u32,
    start: String,
    best: Option<f64>,
    /// An explore worker's seed for generating its idea.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seed: Option<String>,
}

/// A candidate at a gate: an accepted explore claim with its best state and verdicts.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Candidate {
    idea: u32,
    claim: String,
    snap: String,
    search: Option<f64>,
    select: Option<f64>,
    good: bool,
    audit: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    reason: String,
}

/// Journal records. An episode is `started` before its child runs and `finished` after; resume closes one that never
/// finished with a durable charge from the killed child's journals (its whole allowance when its scratch directory is
/// gone), runs what was planned but never started, and continues. Held-out scores, audit verdicts and gates are
/// journaled once and reused.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Planned { batch: String, episodes: Vec<Plan> },
    Started { label: String, plan: String, allowance: f64, #[serde(default)] scratch: Option<PathBuf> },
    Finished { label: String, cost: f64, evals: usize, #[serde(default)] interrupted: bool, #[serde(default)] crashed: bool },
    Selected { snap: String, score: Option<f64> },
    Audited { idea: u32, snap: String, pass: bool, reason: String },
    Gate { round: usize, candidates: Vec<Candidate>, chosen: Vec<u32> },
}

/// What one episode spent and did.
struct Outcome {
    cost: f64,
    crashed: bool,
    stderr: String,
}

struct Orchestrator {
    workspace: PathBuf,
    dir: PathBuf,
    launch: Launch,
    spent: f64,
}

impl Orchestrator {
    fn spec(&self) -> &Spec { &self.launch.spec }

    fn journal(&self, record: &Record) -> Result<()> {
        append_line(&self.dir.join("journal.jsonl"), record)
    }

    fn records(&self) -> Result<Vec<Record>> {
        read_lines(&self.dir.join("journal.jsonl"))
    }

    fn seed(&self) -> Result<String> {
        Ok(std::fs::read_to_string(self.dir.join("seed"))?.trim().to_string())
    }

    /// Every episode gets at most an equal share of the budget: the research, each round's workers and each gate's audits.
    fn share(&self) -> f64 {
        self.launch.budget / (1 + PACE * (WORKERS + TOP)) as f64
    }

    /// Score the seed with the search evaluator before any episode, so every episode knows where it starts. Kept in
    /// `seed.json` (written whole or not at all), outside the ledger, which holds the agents' evaluations.
    async fn baseline(&self) -> Result<()> {
        let (score, valid, _, report) = evaluate(self.spec(), &self.dir, &load_snap(&self.dir, &self.seed()?)?, &self.spec().eval, false).await?;
        eprintln!("hx ar: seed scores {}", if valid { show(score) } else { "invalid".into() });
        let tmp = self.dir.join("seed.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&json!({"score": score.filter(|_| valid), "report": report}))?)?;
        std::fs::rename(&tmp, self.dir.join("seed.json"))?;
        Ok(())
    }

    /// The search score and report of `snap`: its first valid evaluation, or the host's baseline for the seed.
    fn known(&self, rows: &[Row], snap: &str) -> (Option<f64>, Value) {
        if let Some(r) = rows.iter().find(|r| r.snap == snap && r.valid) { return (r.score, r.report.clone()); }
        let baseline = (self.seed().ok().as_deref() == Some(snap)).then(|| std::fs::read(self.dir.join("seed.json")).ok()).flatten()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok()).unwrap_or(Value::Null);
        (baseline["score"].as_f64(), baseline["report"].clone())
    }

    /// The held-out score of `snap` (none without a select evaluator, or when it is invalid), journaled once.
    async fn select(&self, snap: &str) -> Result<Option<f64>> {
        let Some(cmd) = self.spec().select.clone() else { return Ok(None) };
        if let Some(score) = self.records()?.into_iter().find_map(|r| match r { Record::Selected { snap: s, score } if s == snap => Some(score), _ => None }) {
            return Ok(score);
        }
        let (score, valid, _, _) = evaluate(self.spec(), &self.dir, &load_snap(&self.dir, snap)?, &cmd, true).await?;
        let score = score.filter(|_| valid);
        self.journal(&Record::Selected { snap: snap.into(), score })?;
        Ok(score)
    }

    /// The final answer of the last finished attempt of a planned episode: a worker's finding, the brief, a verdict.
    fn answer(&self, records: &[Record], plan: &str) -> Option<String> {
        let label = records.iter().rev().find_map(|r| match r {
            Record::Started { label, plan: p, .. } if p == plan => Some(label.clone()),
            _ => None,
        })?;
        let report: Value = serde_json::from_slice(&std::fs::read(self.dir.join("episodes").join(format!("{label}.report.json"))).ok()?).ok()?;
        report["final_answer"].as_str().map(str::trim).filter(|a| !a.is_empty()).map(String::from)
    }

    /// Workers per round: `WORKERS`, or `NARROW` once any episode's journal records a rate limit (HTTP 429).
    fn width(&self) -> usize {
        let limited = |dir: PathBuf| -> bool {
            std::fs::read_dir(dir).into_iter().flatten().flatten().any(|f| {
                f.path().extension().is_some_and(|e| e == "jsonl")
                    && std::fs::read_to_string(f.path()).is_ok_and(|t| t.contains("\"API 429 "))
            })
        };
        let logs = std::fs::read_dir(self.dir.join("episodes")).into_iter().flatten().flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".log"));
        for log in logs {
            if limited(log.path()) {
                eprintln!("hx ar: the provider rate-limited {}; rounds run {NARROW} workers", log.file_name().to_string_lossy());
                return NARROW;
            }
        }
        WORKERS
    }

    /// The episodes of a journaled batch.
    fn planned(&self, batch: &str) -> Result<Option<Vec<Plan>>> {
        Ok(self.records()?.into_iter().find_map(|r| match r { Record::Planned { batch: b, episodes } if b == batch => Some(episodes), _ => None }))
    }

    /// Journal a batch once.
    fn plan(&self, batch: &str, episodes: Vec<Plan>) -> Result<()> {
        if !self.records()?.iter().any(|r| matches!(r, Record::Planned { batch: b, .. } if b == batch)) {
            self.journal(&Record::Planned { batch: batch.into(), episodes })?;
        }
        Ok(())
    }

    /// Run a planned batch side by side until every episode has finished, or crashed `MAX_CRASHES` times. Each runs with
    /// at most its equal share, or an equal split of what is left.
    async fn run_batch(&mut self, batch: &str) -> Result<()> {
        let mut crashes = 0;
        loop {
            let records = self.records()?;
            let plans = self.planned(batch)?.unwrap_or_default();
            let attempts = |plan: &str| -> Vec<(&String, bool)> {
                records.iter().filter_map(|r| match r { Record::Started { label, plan: p, .. } if p == plan => Some(label), _ => None })
                    .map(|label| (label, records.iter().any(|f| matches!(f, Record::Finished { label: l, crashed: true, .. } if l == label))))
                    .collect()
            };
            let pending: Vec<(Plan, String)> = plans.into_iter().filter_map(|plan| {
                let tries = attempts(&plan.label);
                let done = tries.iter().any(|(_, crashed)| !crashed) || tries.len() >= MAX_CRASHES as usize;
                let label = if tries.is_empty() { plan.label.clone() } else { format!("{}.{}", plan.label, tries.len() + 1) };
                (!done).then_some((plan, label))
            }).collect();
            let retries_left = pending.iter().any(|(plan, _)| attempts(&plan.label).len() + 1 < MAX_CRASHES as usize);
            if pending.is_empty() { return Ok(()); }
            let remaining = self.launch.budget - self.spent;
            // Less than half a share cannot fund a useful episode.
            if remaining < self.share() / 2.0 { return Ok(()); }
            // The research brief serves every later episode, so it gets three shares.
            let share = if batch == "research" { 3.0 * self.share() } else { self.share() };
            let allowance = share.min(remaining / pending.len() as f64);
            let state = self.state()?;
            let mut runs = vec![];
            for (plan, label) in &pending {
                runs.push(self.episode(plan, label, allowance, self.task(plan, label, allowance, &state)?));
            }
            let outcomes = futures::future::join_all(runs).await.into_iter().collect::<Result<Vec<_>>>()?;
            self.spent += outcomes.iter().map(|o| o.cost).sum::<f64>();
            if !outcomes.iter().all(|o| o.crashed) { crashes = 0; continue; }
            crashes += 1;
            // Before the first eval of the whole run, repeated crashes mean misconfiguration: stop, don't wait hours.
            if ledger(&self.dir)?.is_empty() && crashes >= MAX_CRASHES {
                let last: Vec<&str> = outcomes[0].stderr.trim().lines().rev().take(15).collect();
                bail!("episode children keep failing before any eval; last error:\n{}", last.into_iter().rev().collect::<Vec<_>>().join("\n"));
            }
            if !retries_left { return Ok(()); }
            let base = std::env::var("HX_AR_RETRY_SECS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(60);
            let pause = base << crashes;
            eprintln!("hx ar: every episode of {batch} crashed before any eval; retrying in {pause}s");
            tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
        }
    }

    /// One fresh child run in its own copy of the base holding its starting state.
    async fn episode(&self, plan: &Plan, label: &str, allowance: f64, task: String) -> Result<Outcome> {
        let ep_dir = self.dir.join("episodes");
        std::fs::create_dir_all(&ep_dir)?;
        let ep_path = ep_dir.join(format!("{label}.json"));
        let episode = Episode { dir: self.dir.clone(), label: label.into(), spec: self.spec().clone(), role: plan.role,
            idea: plan.idea, start: plan.start.clone(), best: plan.best };
        std::fs::write(&ep_path, serde_json::to_vec_pretty(&episode)?)?;
        let task_path = ep_dir.join(format!("{label}.md"));
        std::fs::write(&task_path, task)?;
        let scratch = Scratch::new("ws")?;
        let ws = scratch.0.join("ws");
        copy_tree(&self.dir.join("base"), &ws)?;
        place(&ws, self.spec(), &load_snap(&self.dir, &plan.start)?)?;
        self.journal(&Record::Started { label: label.into(), plan: plan.label.clone(), allowance, scratch: Some(scratch.0.clone()) })?;
        eprintln!("hx ar: {label} ({:?}{}, ${allowance:.2})", plan.role, if plan.idea > 0 { format!(" #{}", plan.idea) } else { String::new() });
        let out = tokio::process::Command::new(std::env::current_exe()?)
            .arg("run").arg("--workspace").arg(&ws).args(&self.launch.child_args)
            .arg("--prompt-file").arg(&task_path)
            .arg("--max-cost").arg(format!("{allowance:.6}"))
            .env("HX_AR_EPISODE", &ep_path)
            .stdin(std::process::Stdio::null())
            .output().await?;
        std::fs::write(ep_dir.join(format!("{label}.stderr")), &out.stderr)?;
        let report = String::from_utf8_lossy(&out.stdout).lines().rev()
            .find_map(|l| serde_json::from_str::<Value>(l).ok()).unwrap_or(Value::Null);
        std::fs::write(ep_dir.join(format!("{label}.report.json")), serde_json::to_vec_pretty(&report)?)?;
        let logs = keep_logs(&ws, &ep_dir, label)?;
        drop(scratch);
        let cost = report["cost_usd"].as_f64().unwrap_or_else(|| logged_cost(&logs, allowance));
        let evals = ledger(&self.dir)?.iter().filter(|r| r.episode == label).count();
        let crashed = evals == 0 && report.is_null();
        self.journal(&Record::Finished { label: label.into(), cost, evals, interrupted: false, crashed })?;
        let end = if report["finished"] == true { "finished" } else { report["stopped"].as_str().unwrap_or("stopped") };
        eprintln!("hx ar: {label}: {evals} evals, ${cost:.3}, {end}");
        Ok(Outcome { cost, crashed, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
    }

    fn state(&self) -> Result<State> {
        let records = self.records()?;
        let rows = ledger(&self.dir)?;
        let claims: Vec<Claim> = read_lines(&self.dir.join("claims.jsonl"))?;
        let brief = self.answer(&records, "r0-research").unwrap_or_else(|| "(none: the research episode wrote no brief)".into());
        // A finding per finished worker attempt: its claim, best score and final answer; the most recent are shown.
        let mut findings = vec![];
        for r in &records {
            let Record::Finished { label, crashed: false, .. } = r else { continue };
            if !label.starts_with('r') || label.starts_with("r0") { continue; }
            let claim = claims.iter().rev().find(|c| c.worker == *label).map_or("(no accepted claim)".to_string(), |c| match c.scope.as_str() {
                "explore" => format!("idea #{}: {}", c.n, c.idea),
                scope => format!("refinement of #{}: {}", &scope[1..], c.idea),
            });
            let best = rows.iter().filter(|x| x.episode == *label && x.improved).filter_map(|x| x.score)
                .fold(None, |b: Option<f64>, s| if self.spec().better(s, b, 0.0) { Some(s) } else { b });
            let text = std::fs::read(self.dir.join("episodes").join(format!("{label}.report.json"))).ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v["final_answer"].as_str().map(String::from)).unwrap_or_default();
            findings.push(format!("### {label}: {} (best {})\n\n{}\n\n", claim.replace('\n', " "), show(best),
                clip(text.trim(), FINDING_CHARS)));
        }
        let skip = findings.len().saturating_sub(FINDINGS);
        let findings = if skip > 0 { format!("({skip} earlier findings omitted)\n\n") } else { String::new() } + &findings[skip..].concat();
        Ok(State { rows, claims, brief, findings })
    }

    fn protocol(&self, role: Role, allowance: f64) -> String {
        let spec = self.spec();
        let better = if spec.direction == "max" { "higher" } else { "lower" };
        let mut t = format!("- `ar.eval(\"one-line note\")` scores the current editable files ({}) with the owner's frozen evaluator and \
returns {{n, score, valid, se, improved, best, report}}; {better} scores are better. Only states scored by ar.eval count.\n\
- `ar` is a built-in global table like `sh` and `read`. `vars()` lists only your own globals, so it never shows `ar`: call it directly.\n\
- When the evaluator reports no `se`, it is deterministic: re-scoring a state returns its stored result with `cached: true`.\n",
            spec.editable.join(", "));
        if matches!(role, Role::Explore | Role::Exploit) {
            t += "- `improved` means better than your best so far (by more than `se` when the evaluator reports one)";
            if spec.min_delta > 0.0 { t += &format!(". To become a result, a candidate must beat the starting seed by more than {}", spec.min_delta); }
            t += ". `ar.reset()` puts your best scored state back into the editable paths: use it instead of reverting by hand.\n\
- `ar.claims()` lists the claims made so far that yours must differ from: read it before you claim.
- Score with ar.eval after each change and read the report: it shows what broke. One change per eval, so the ledger stays readable.\n";
        } else {
            t += "- Your evaluations are probes: recorded, never a candidate. Change the editable files freely to test a question.\n";
        }
        if spec.select.is_some() {
            t += "- Candidates are judged on held-out data no agent sees. Prefer changes with a reason to generalize over tuning to the reported numbers.\n";
        }
        t + &format!("- This episode has about ${allowance:.2} of model budget.\n")
    }

    fn task(&self, plan: &Plan, label: &str, allowance: f64, state: &State) -> Result<String> {
        let spec = self.spec();
        let (score, report) = self.known(&state.rows, &plan.start);
        let report = clip(&serde_json::to_string_pretty(&report).unwrap_or_default(), REPORT_CHARS);
        let program = self.launch.program.trim_end();
        let brief = format!("## Brief from the research lead\n\n{}\n\n", clip(&state.brief, BRIEF_CHARS));
        let findings = if state.findings.is_empty() { String::new() } else { format!("## Findings so far\n\n{}", state.findings) };
        let claim = |idea: u32| state.claims.iter().find(|c| c.scope == "explore" && c.n == idea).map_or(String::new(), |c| c.idea.clone());
        let starting = format!("## Starting state\n\nYour workspace holds it (search score {}). Its evaluator report:\n\n```json\n{report}\n```\n", show(score));
        Ok(match plan.role {
            Role::Research => format!("{program}\n\n## Autoresearch research lead ({label})\n\n\
You lead an autoresearch run. Next, {WORKERS} workers will each pursue a different idea to improve the score by changing the \
editable paths ({}). Study the task first, so their work is grounded, and write the brief every worker gets:\n\
- the objective, and exactly how the evaluator computes the score (read the code and data you can see);\n\
- rules and constraints: what is banned, what makes a result invalid;\n\
- what the starting state does and where it is weak;\n\
- traps: ways a score could rise without real improvement (evaluator quirks, overfitting, degenerate solutions) that workers must avoid;\n\
- open questions worth testing.\n\
Do not solve the task: the workers do that. Keep the study short, about 15 steps: an unfinished study leaves every \
worker without a brief. Finish with the brief as your final answer, at most 800 words, facts first, without prescribing \
one solution.\n\n{}\n{starting}", spec.editable.join(", "), self.protocol(plan.role, allowance)),
            Role::Explore => {
                // Only earlier rounds' claims: a retried worker gets its own claim back from ar.claim and must keep building it.
                let round = label.split('-').next();
                let taken: String = state.claims.iter().filter(|c| c.scope == "explore" && c.worker.split('-').next() != round)
                    .map(|c| format!("- #{}: {}\n", c.n, c.idea)).collect();
                let taken = if taken.is_empty() { String::new() } else {
                    format!("\n\nEarlier rounds already claimed these ideas; yours must use a different core approach:\n{}", clip(&taken, BRIEF_CHARS))
                };
                format!("{program}\n\n{brief}{findings}## Autoresearch explore worker {label}\n\n\
You are one of {WORKERS} workers exploring side by side. Each tests a different core approach to improving the score.\n\n\
1. Generate your idea from your seed: <seed>{}</seed>. List 5 to 7 root-level dimensions along which approaches to this \
task differ fundamentally (not parameters or filters), each with at least 6 options from conventional to radical. Pick one \
option per dimension using a different part of the seed (for example the sum of the character codes of a segment, mod the \
number of options). Commit to the picks even if unusual, and compose one coherent idea: the core approach to build, not a \
tweak on top of the starting state.{taken}\n\
2. Claim it early, within your first few steps and before building: `ar.claim(\"the idea in two or three sentences\")`. \
Claims are served first come, first served. If it returns `ok = false`, another worker already claimed the same core \
approach (`message` lists every claim): pick again from a different part of your seed and claim a different core. An \
accepted claim is final, so claim only your real idea, never a placeholder or a test. ar.eval works only after an \
accepted claim.\n\
3. Build the idea faithfully, score it with ar.eval and read the report. Fix what is broken (errors, invalid results, a bug \
that keeps the idea from working as designed) until it works. Do not optimize: no parameter sweeps, no tuning for score, \
no switching ideas. Later rounds refine the strongest candidates, so return as soon as the idea works. Your first valid \
state sets your bar, even below the starting state's score.\n\
4. Finish with your finding as the final answer: the idea, what you built, your best score, what worked and what did not, \
and whether the idea is promising.\n\n{}\n{starting}", plan.seed.as_deref().unwrap_or(label), self.protocol(plan.role, allowance))
            }
            Role::Exploit => {
                let rows: Vec<&Row> = state.rows.iter().filter(|r| r.idea == plan.idea).collect();
                let skip = rows.len().saturating_sub(SHOWN);
                let mut ledger = format!("n | score | improved | worker | note\n{}", if skip > 0 { format!("({skip} older rows omitted)\n") } else { String::new() });
                for r in &rows[skip..] {
                    let score = match (r.valid, r.score) { (true, Some(s)) => format!("{s:.6}"), _ => "invalid".into() };
                    ledger += &format!("{} | {score} | {} | {} | {}\n", r.n, if r.improved { "yes" } else { "" }, r.episode, r.note.replace('\n', " "));
                }
                format!("{program}\n\n{brief}{findings}## Autoresearch exploit worker {label}: candidate #{}\n\n\
Candidate #{}'s idea: {}\n\nIt is among the most promising candidates. Several workers refine it side by side; improve it:\n\n\
1. Choose one refinement, a change with a reason to generalize rather than tuning to the reported numbers, and claim it: \
`ar.claim(\"the refinement in one or two sentences\")`, early. If it returns `ok = false`, another worker claimed the \
same refinement first (`message` lists every claim): choose a different one. An accepted claim is final, so claim only \
your real refinement, never a placeholder or a test. ar.eval works only after an accepted claim.\n\
2. Implement it, ar.eval, read the report, and fix it if it is broken. Do not optimize beyond your one refinement: no \
parameter sweeps, no further changes. Return once it is built and scored; the next round builds on the best result. An \
improvement must beat the candidate's best.\n\
3. Finish with your finding as the final answer: what you changed, your best score, what worked and what did not.\n\n{}\n\
## Ledger of candidate #{}\n\n{ledger}\n{starting}", plan.idea, plan.idea, claim(plan.idea),
                    self.protocol(plan.role, allowance), plan.idea)
            }
            Role::Audit => format!("{program}\n\n{brief}{findings}## Autoresearch auditor {label}: candidate #{}\n\n\
Candidate #{} ({}) is about to be kept as a result. Your workspace holds its best state. Decide whether its score is earned:\n\
- Read the state and explain where its score comes from.\n\
- Look for an evaluator hole: fills, prices or outcomes the real system would not give, lookahead or leaked information, \
data the solution should not see, rule violations, or degenerate behaviour the score rewards by mistake.\n\
- To test a suspicion, write probe states (for example the same trick in isolation) and ar.eval them.\n\
Overfitting alone is not a failure: held-out data handles that. Finish with your reasoning and a last line \
`VERDICT: PASS` (the score is earned) or `VERDICT: FAIL: reason` (it comes from a hole or a rule violation).\n\n{}\n{starting}",
                plan.idea, plan.idea, claim(plan.idea), self.protocol(plan.role, allowance)),
        })
    }

    /// Candidates after `round`: every accepted explore claim with a valid improved state, at its best search state.
    /// Good: beats the seed on search by more than `min_delta`, and is not worse than the seed on select (a veto, never
    /// a ranking). The `TOP` strongest go on (good ones first, then by search; one per distinct state): workers return
    /// their ideas unpolished, so one below the seed still earns refinement. Good ones among them are audited until
    /// `TOP` pass or none is left; only good ones can become results.
    async fn gate(&mut self, round: usize) -> Result<Vec<u32>> {
        let seed = self.seed()?;
        let seed_search = self.known(&ledger(&self.dir)?, &seed).0;
        let seed_select = self.select(&seed).await?;
        let prefix = format!("audit{round}.");
        // Set when an audit batch could not start (the budget is spent): the gate then goes on with its candidates
        // unaudited, and says so, rather than inventing a verdict.
        let mut spent = false;
        loop {
            // A resumed run first finishes the audit batch it was in: verdicts come from that batch's own plans.
            let records = self.records()?;
            let unjudged = records.iter().find_map(|r| match r {
                Record::Planned { batch, episodes } if batch.starts_with(&prefix) && episodes.iter().any(|p| !audited(&records, p)) =>
                    Some((batch.clone(), episodes.clone())),
                _ => None,
            });
            if let Some((batch, plans)) = unjudged.filter(|_| !spent) {
                self.run_batch(&batch).await?;
                spent = self.judge_audits(&plans)? == 0;
                continue;
            }
            let state = self.state()?;
            let mut candidates = vec![];
            for c in state.claims.iter().filter(|c| c.scope == "explore") {
                let best = state.rows.iter().filter(|r| r.idea == c.n && r.improved && r.valid)
                    .fold(None::<&Row>, |b, r| if self.spec().better(r.score.unwrap(), b.and_then(|b| b.score), 0.0) { Some(r) } else { b });
                let Some(best) = best else { continue };
                let beats = seed_search.is_none_or(|s| self.spec().better(best.score.unwrap(), Some(s), self.spec().min_delta));
                // The veto is computed only where it can decide something, so held-out data is looked at as little as possible.
                let select = if beats { self.select(&best.snap).await? } else { None };
                let held = self.spec().select.is_none()
                    || select.is_some_and(|s| seed_select.is_none_or(|b| !self.spec().better(b, Some(s), 0.0)));
                let audit = records.iter().rev().find_map(|r| match r {
                    Record::Audited { idea, snap, pass, reason } if *idea == c.n && *snap == best.snap => Some((*pass, reason.clone())),
                    _ => None,
                });
                candidates.push(Candidate { idea: c.n, claim: c.idea.clone(), snap: best.snap.clone(), search: best.score, select,
                    good: beats && held, audit: audit.as_ref().map(|a| a.0), reason: audit.map(|a| a.1).unwrap_or_default() });
            }
            // A candidate that scores exactly as the seed is the seed in other bytes: it never goes on.
            let seedlike = |c: &Candidate| c.search == seed_search;
            let mut ranked: Vec<&Candidate> = candidates.iter().filter(|c| c.audit != Some(false) && !seedlike(c)).collect();
            ranked.sort_by(|a, b| b.good.cmp(&a.good).then_with(|| {
                let (a, b) = (a.search.unwrap_or(f64::NAN), b.search.unwrap_or(f64::NAN));
                if self.spec().better(a, Some(b), 0.0) { std::cmp::Ordering::Less } else if self.spec().better(b, Some(a), 0.0) { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal }
            }));
            // Two candidates that reached the same state are one result: the better ranked keeps it.
            let mut seen = std::collections::HashSet::new();
            ranked.retain(|c| seen.insert(c.snap.clone()));
            let unaudited: Vec<&Candidate> = ranked.iter().take(TOP).filter(|c| c.good && c.audit.is_none()).copied().collect();
            if unaudited.is_empty() || spent {
                let unaudited: Vec<u32> = unaudited.iter().map(|c| c.idea).collect();
                // An unaudited good candidate never ranks above an audited one, so it reaches the workspace only when
                // nothing audited did; candidates below the seed come last.
                let mut chosen: Vec<&Candidate> = ranked.iter().take(TOP).copied().collect();
                chosen.sort_by_key(|c| (!c.good, unaudited.contains(&c.idea)));
                let chosen: Vec<u32> = chosen.iter().map(|c| c.idea).collect();
                for c in candidates.iter_mut().filter(|c| unaudited.contains(&c.idea)) { c.reason = "not audited: budget spent".into(); }
                eprintln!("hx ar: gate {round}: {} candidates, {} good, going on with {:?}", candidates.len(),
                    candidates.iter().filter(|c| c.good).count(), chosen);
                self.journal(&Record::Gate { round, candidates, chosen: chosen.clone() })?;
                return Ok(chosen);
            }
            let k = records.iter().filter(|r| matches!(r, Record::Planned { batch, .. } if batch.starts_with(&prefix))).count() + 1;
            let plans: Vec<Plan> = unaudited.iter().map(|c| Plan { label: format!("a{round}.{k}-c{}", c.idea), role: Role::Audit,
                idea: c.idea, start: c.snap.clone(), best: None, seed: None }).collect();
            self.plan(&format!("{prefix}{k}"), plans.clone())?;
            self.run_batch(&format!("{prefix}{k}")).await?;
            spent = self.judge_audits(&plans)? == 0;
        }
    }

    /// Journal the verdict of each audit plan that has none yet and ran, from its last attempt's final answer; returns
    /// how many were judged (none when the budget left no room to start them).
    fn judge_audits(&self, plans: &[Plan]) -> Result<usize> {
        let records = self.records()?;
        // An attempt of the plan finished without crashing: a crashed auditor audited nothing.
        let ran = |p: &Plan| records.iter().any(|r| matches!(r, Record::Started { label, plan, .. } if *plan == p.label
            && records.iter().any(|f| matches!(f, Record::Finished { label: l, crashed: false, .. } if l == label))));
        let mut judged = 0;
        for plan in plans.iter().filter(|p| !audited(&records, p) && ran(p)) {
            judged += 1;
            let (pass, reason) = verdict(self.answer(&records, &plan.label).as_deref());
            eprintln!("hx ar: audit of #{}: {}{}", plan.idea, if pass { "pass" } else { "FAIL" }, if reason.is_empty() { String::new() } else { format!(" ({reason})") });
            self.journal(&Record::Audited { idea: plan.idea, snap: plan.start.clone(), pass, reason })?;
        }
        Ok(judged)
    }

    async fn run(&mut self, mut records: Vec<Record>) -> Result<Value> {
        // Close episodes that never finished, durably.
        let rows = ledger(&self.dir)?;
        let open: Vec<(String, f64, Option<PathBuf>)> = records.iter().filter_map(|r| match r {
            Record::Started { label, allowance, scratch, .. }
                if !records.iter().any(|f| matches!(f, Record::Finished { label: l, .. } if l == label)) =>
                Some((label.clone(), *allowance, scratch.clone())),
            _ => None,
        }).collect();
        for (label, allowance, scratch) in open {
            let evals = rows.iter().filter(|r| r.episode == label).count();
            // The killed child's scratch copy survives a killed orchestrator: keep its journals, charge from them. Without
            // its scratch copy nothing is known, so the whole allowance is charged.
            let cost = match scratch.filter(|s| s.is_dir()) {
                Some(dir) => {
                    let logs = keep_logs(&dir.join("ws"), &self.dir.join("episodes"), &label)?;
                    drop(Scratch(dir));
                    logged_cost(&logs, allowance)
                }
                None => allowance,
            };
            // Research and audits are judged by their final answer, which a killed child never gave: they are retried
            // like a worker that scored nothing.
            let answer_only = label.starts_with("r0-") || label.starts_with('a');
            let record = Record::Finished { label, cost, evals, interrupted: true, crashed: evals == 0 || answer_only };
            self.journal(&record)?;
            records.push(record);
        }
        self.spent += records.iter().map(|r| match r { Record::Finished { cost, .. } => *cost, _ => 0.0 }).sum::<f64>();
        if !self.dir.join("seed.json").exists() { self.baseline().await?; }
        let seed = self.seed()?;
        // A select evaluator that cannot score the seed would reject every candidate at the first gate: stop before spending.
        if self.spec().select.is_some() {
            ensure!(self.select(&seed).await?.is_some(), "the select evaluator gives no valid score for the seed; fix it before searching");
        }
        self.plan("research", vec![Plan { label: "r0-research".into(), role: Role::Research, idea: 0, start: seed.clone(), best: None, seed: None }])?;
        self.run_batch("research").await?;
        // Rounds go on while the budget lasts. Round 1 explores; later rounds exploit the last gate's chosen candidates
        // while that moves them, and explore new ideas after an exploit round that moved none.
        let (mut last, mut explore) = (Vec::<(u32, String)>::new(), true);
        for round in 1.. {
            let batch = format!("round{round}");
            let mut gate = self.records()?.into_iter().find_map(|r| match r { Record::Gate { round: g, candidates, chosen } if g == round => Some((candidates, chosen)), _ => None });
            let plans = match self.planned(&batch)? {
                Some(plans) => plans,
                None if gate.is_some() => vec![],
                None => {
                    // Each worker gets a full share and one share stays for the gate's audits; the last round runs fewer workers.
                    let s = self.share();
                    let workers = (((self.launch.budget - self.spent - s) / s).round().max(0.0) as usize).min(self.width());
                    if workers == 0 { break; }
                    let plans = if explore {
                        (1..=workers).map(|w| -> Result<Plan> {
                            Ok(Plan { label: format!("r{round}-w{w}"), role: Role::Explore, idea: 0, start: seed.clone(), best: None,
                                seed: Some(crate::ssot::seed()?) })
                        }).collect::<Result<Vec<_>>>()?
                    } else {
                        let candidates = self.last_gate()?.map(|g| g.0).unwrap_or_default();
                        (0..workers).map(|w| {
                            let (idea, start) = &last[w % last.len()];
                            let best = candidates.iter().find(|c| c.idea == *idea).and_then(|c| c.search);
                            Plan { label: format!("r{round}-w{}", w + 1), role: Role::Exploit, idea: *idea, start: start.clone(), best, seed: None }
                        }).collect()
                    };
                    self.plan(&batch, plans.clone())?;
                    plans
                }
            };
            if gate.is_none() {
                self.run_batch(&batch).await?;
                self.gate(round).await?;
                gate = self.last_gate()?;
            }
            let (candidates, chosen) = gate.unwrap_or_default();
            // With no candidate after a gate the run stops (in practice after round 1: later gates keep earlier candidates).
            if chosen.is_empty() { break; }
            let now: Vec<(u32, String)> = chosen.iter().filter_map(|id| candidates.iter().find(|c| c.idea == *id)).map(|c| (c.idea, c.snap.clone())).collect();
            let moved = now.iter().any(|c| !last.contains(c));
            // Exploring follows only an exploit round that moved nothing: when exploring moves nothing either, both ways
            // forward are spent.
            if !moved && plans.first().is_some_and(|p| p.role == Role::Explore) {
                eprintln!("hx ar: neither refining nor exploring moved a chosen candidate; stopping");
                break;
            }
            explore = !moved;
            last = now;
        }
        let state = self.state()?;
        ensure!(!state.rows.is_empty(), "autoresearch made no evaluations; see {}", self.dir.join("episodes").display());
        let (candidates, chosen) = self.last_gate()?.unwrap_or_default();
        let portfolio: Vec<&Candidate> = chosen.iter().filter_map(|id| candidates.iter().find(|c| c.idea == *id)).filter(|c| c.good).collect();
        // The portfolio's states, best search score first; the first goes into the workspace (the seed when none).
        for c in &portfolio {
            let dir = self.dir.join("portfolio").join(format!("c{}", c.idea));
            if !dir.exists() { place(&dir, self.spec(), &load_snap(&self.dir, &c.snap)?)?; }
        }
        let (snap, search, select) = match portfolio.first() {
            Some(c) => (c.snap.clone(), c.search, c.select),
            None => (seed.clone(), self.known(&state.rows, &seed).0, self.select(&seed).await?),
        };
        place(&self.workspace, self.spec(), &load_snap(&self.dir, &snap)?)?;
        if portfolio.is_empty() { eprintln!("hx ar: no candidate beat the seed; the workspace keeps the seed"); }
        let report = json!({
            "run": self.dir.file_name().unwrap().to_string_lossy(), "dir": self.dir, "commit": self.launch.commit,
            "snap": snap, "search": search, "select": select, "portfolio": portfolio, "candidates": candidates,
            "rounds": self.records()?.iter().filter(|r| matches!(r, Record::Gate { .. })).count(),
            "evals": state.rows.len(), "cost_usd": self.spent, "budget_usd": self.launch.budget,
        });
        std::fs::write(self.dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        Ok(report)
    }

    /// The last gate's candidates and the ideas it chose.
    fn last_gate(&self) -> Result<Option<(Vec<Candidate>, Vec<u32>)>> {
        Ok(self.records()?.into_iter().rev().find_map(|r| match r { Record::Gate { candidates, chosen, .. } => Some((candidates, chosen)), _ => None }))
    }
}

/// Has this audit plan's verdict been journaled?
fn audited(records: &[Record], plan: &Plan) -> bool {
    records.iter().any(|r| matches!(r, Record::Audited { idea, snap, .. } if *idea == plan.idea && *snap == plan.start))
}

/// What the journal, ledger and claims say, as workers are shown it.
struct State {
    rows: Vec<Row>,
    claims: Vec<Claim>,
    brief: String,
    findings: String,
}

/// An auditor's verdict from its last `VERDICT:` line. An auditor that stopped before giving one does not block the
/// candidate; the report says so.
fn verdict(answer: Option<&str>) -> (bool, String) {
    let Some(answer) = answer else { return (true, "no verdict: the auditor stopped before giving one".into()) };
    let upper = answer.to_ascii_uppercase();
    let Some(at) = upper.rfind("VERDICT:") else { return (true, "no verdict: the auditor stopped before giving one".into()) };
    let rest = answer[at + "VERDICT:".len()..].trim();
    if rest.to_ascii_uppercase().starts_with("FAIL") {
        (false, rest[4..].trim_start_matches([':', ' ', '-']).lines().next().unwrap_or("").chars().take(300).collect())
    } else {
        (true, String::new())
    }
}

/// At most `max` characters of `text`, marked when cut.
fn clip(text: &str, max: usize) -> String {
    let cut: String = text.chars().take(max).collect();
    if cut.len() < text.len() { cut + "\n... (truncated)" } else { cut }
}

/// Copy an episode workspace's journals into the run as `episodes/LABEL.log`; returns that directory.
fn keep_logs(ws: &Path, episodes: &Path, label: &str) -> Result<PathBuf> {
    let logs = episodes.join(format!("{label}.log"));
    if ws.join(".harness/log").is_dir() && !logs.exists() {
        std::fs::create_dir_all(episodes)?;
        copy_tree(&ws.join(".harness/log"), &logs)?;
    }
    Ok(logs)
}

/// What a child without a report spent: its journals' settled cost plus unsettled reservations. A child opens its
/// journal before it can admit any model request, so no journal means nothing spent; an unreadable journal is
/// charged `allowance`.
fn logged_cost(logs: &Path, allowance: f64) -> f64 {
    std::fs::read_dir(logs).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .map(|j| journal_cost(&j).unwrap_or(allowance)).sum()
}

/// Settled model cost of a run journal plus its unsettled reservations.
fn journal_cost(log: &Path) -> Option<f64> {
    let books = crate::eventlog::Books::of(&crate::eventlog::load(log).ok()?).ok()?;
    let pending: f64 = books.reservations.values().map(|r| match r.cost {
        crate::eventlog::ReservationCost::UpperBound(c) | crate::eventlog::ReservationCost::Allowance(c) => c,
    }).sum();
    Some(books.usage.cost + pending)
}

fn show(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.6}")).unwrap_or_else(|| "none".into())
}

/// `hx run --ar`: search over the whole `--max-cost` budget, or continue the run in `resume` (a run directory).
pub async fn orchestrate(workspace: &Path, start: Option<(Vec<String>, String, f64)>, resume: Option<PathBuf>) -> Result<()> {
    // A resumed run uses the spec frozen in its sealed launch.json, approved when it started; only a launch reads ar.toml.
    let (dir, launch, records) = match (start, resume) {
        (_, Some(dir)) => {
            let dir = dir.canonicalize().with_context(|| format!("autoresearch run {}", dir.display()))?;
            ensure!(dir.parent() == Some(&workspace.join(".harness/ar").canonicalize()?), "--resume for --ar takes a run directory under .harness/ar");
            let launch: Launch = serde_json::from_slice(&std::fs::read(dir.join("launch.json"))?)?;
            ensure!(!dir.join("report.json").exists(), "autoresearch run {} already finished", dir.display());
            ensure!(dir.join("seed.json").exists() || !dir.join("ledger.jsonl").exists(),
                "autoresearch run {} predates this design and cannot be resumed; start a new run", dir.display());
            let records = read_lines(&dir.join("journal.jsonl"))?;
            (dir, launch, records)
        }
        (Some((child_args, program, budget)), None) => {
            ensure!(crate::trust::is_trusted(workspace), "--ar runs the evaluator commands in .harness/ar.toml; review them and run `hx trust` first");
            ensure!(crate::workflow::git_inventory(workspace)?.is_some(), "--ar requires a Git repository at the workspace root");
            let spec = Spec::load(workspace)?;
            let bad = uncommitted(workspace, &spec)?;
            ensure!(bad.is_empty(), "--ar freezes the committed tree: commit or remove changes outside the editable paths: {}", bad.join(", "));
            let id = format!("{}-{}", chrono_stamp(), std::process::id());
            let dir = workspace.join(".harness/ar").join(&id);
            std::fs::create_dir_all(&dir)?;
            let commit = freeze(workspace, &dir)?;
            std::fs::write(dir.join("seed"), save_snap(&dir, &capture(workspace, &spec)?)?)?;
            let launch = Launch { spec, program, child_args, budget, commit };
            std::fs::write(dir.join("launch.json"), serde_json::to_vec_pretty(&launch)?)?;
            (dir, launch, vec![])
        }
        (None, None) => bail!("--ar needs a program and budget, or --resume RUN_DIR"),
    };
    let budget = launch.budget;
    let mut o = Orchestrator { workspace: workspace.to_path_buf(), dir, launch, spent: 0.0 };
    eprintln!("hx ar: run {} (${budget:.2}){}", o.dir.display(), if records.is_empty() { "" } else { ", resumed" });
    let report = o.run(records).await?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::{answer_number, verdict};

    #[test]
    fn judge_answers() {
        assert_eq!(answer_number("Mechanism: breakout.\nAnswer: 2"), Some(2));
        assert_eq!(answer_number("**Answer:** #3"), Some(3));
        assert_eq!(answer_number("answer: 0"), Some(0));
        assert_eq!(answer_number("It names no approach.\n**Answer:** none"), Some(usize::MAX));
        assert_eq!(answer_number("I think 2 is closest."), None);
    }

    #[test]
    fn audit_verdicts() {
        assert_eq!(verdict(Some("ok\nVERDICT: PASS")), (true, String::new()));
        assert_eq!(verdict(Some("verdict: fail: fills below the market")), (false, "fills below the market".into()));
        assert!(verdict(None).0 && verdict(Some("ran out")).1.starts_with("no verdict"));
    }
}
