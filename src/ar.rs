//! Autoresearch (`hx run --ar`): an agent loop that improves a score by changing a task's editable files.
//!
//! The task supplies `.harness/ar.toml` (editable paths, evaluator commands, grader and holdout roots) and the program
//! text (`--prompt-file`). At launch the committed tree is frozen into the run's `base`. Agents never touch the owner's
//! workspace or its Git repository:
//!
//! - Every episode is a fresh child `hx run` in a throwaway copy of `base` holding the best editable files of its
//!   lineage so far. When an agent stops, a later episode starts fresh from that state and the ledger. Only the
//!   editable paths ever leave an episode.
//! - `ar.eval` snapshots the editable files and scores the snapshot in a second throwaway copy of `base`, so the
//!   evaluator sees exactly the recorded bytes and nothing else the agent did.
//! - Evaluators run under their own sandbox: a scrubbed environment, no network, reads limited to the system,
//!   toolchains, the evaluation copy and the declared grader (and, for `select`, holdout) roots.
//! - The run directory (`.harness/ar/RUN`, with `base`, snapshots, ledger, journal and held-out scores) is sealed from
//!   agents in every mode.
//!
//! The host owns what to try as well as the best states. A lineage is one research direction, sampled with String Seed
//! of Thought so that directions differ, pursued from a starting state with its own best state. A few lineages start
//! from the seed together; afterwards the leading lineage gets the next episode, a lineage that stops improving is
//! retired, and when none is left a new direction starts from the best state found. At the end, the held-out `select`
//! evaluator, which agents never trigger, picks among the seed and every improvement of every lineage.
use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The search stops when less than this share of the budget is left.
const MIN_SLICE: f64 = 0.01;
/// Consecutive rounds of episodes that all finish without an eval before the search stops.
const MAX_IDLE: u32 = 2;
/// Lineages that start from the seed, each with its own direction.
const DRAFTS: usize = 3;
/// Consecutive episodes of a lineage that do not improve it before it is retired.
const PATIENCE: u32 = 2;
/// Characters of the starting state's editable files shown when sampling directions.
const STATE_CHARS: usize = 16_000;
/// Ledger rows shown to an episode, and characters of the best state's evaluator report.
const SHOWN: usize = 40;
const REPORT_CHARS: usize = 12_000;
/// Episodes per run (a guard; the budget normally ends the search first).
const MAX_ATTEMPTS: usize = 100;
/// Retries of a child that crashes before any eval (usually provider rate limits).
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
    /// Selection evaluator (held-out): used only by the orchestrator to pick the final result among checkpoints.
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

/// What a child run needs to serve `ar.eval`; written by the orchestrator, named by `HX_AR_EPISODE`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    /// The run directory; sealed from the child's agent.
    pub dir: PathBuf,
    pub label: String,
    pub spec: Spec,
    /// The lineage this episode works on, the snapshot it started from, and the lineage's best score so far.
    pub lineage: u32,
    pub start: String,
    pub best: Option<f64>,
}
pub static EPISODE: OnceLock<(Episode, Mutex<Option<f64>>)> = OnceLock::new();

/// Called once at startup: a child run of an autoresearch episode gets `ar.eval`, and its policy seals the run.
pub fn load_episode() -> Result<()> {
    if let Some(path) = std::env::var_os("HX_AR_EPISODE") {
        let episode: Episode = serde_json::from_slice(&std::fs::read(&path)?).context("HX_AR_EPISODE")?;
        let best = episode.best;
        let _ = EPISODE.set((episode, Mutex::new(best)));
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
    #[serde(default)]
    lineage: u32,
    note: String,
    snap: String,
    score: Option<f64>,
    valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se: Option<f64>,
    improved: bool,
    /// The search evaluator's report, shown to later episodes for the best state.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    report: Value,
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

/// `ar.eval(note)` inside an episode: snapshot the editable files, score the snapshot, append to the ledger.
pub async fn eval(pol: &crate::policy::Policy, note: String) -> Result<Value> {
    let (episode, best) = EPISODE.get().context("ar.eval is only available inside an autoresearch episode")?;
    let spec = &episode.spec;
    let files = capture(&pol.workspace, spec)?;
    let snap = save_snap(&episode.dir, &files)?;
    // An evaluator that reports no `se` is deterministic: a state it already scored validly gets the stored result,
    // and no new ledger row.
    let rows = ledger(&episode.dir)?;
    if let Some(r) = rows.iter().find(|r| r.snap == snap && r.valid && r.se.is_none()) {
        let best = *best.lock().unwrap();
        return Ok(json!({"n": r.n, "score": r.score, "valid": true, "se": null, "improved": false, "best": best,
            "report": r.report, "cached": true}));
    }
    let (score, valid, se, report) = evaluate(spec, &episode.dir, &files, &spec.eval, false).await?;
    let mut best = best.lock().unwrap();
    let margin = spec.min_delta.max(se.unwrap_or(0.0));
    let improved = valid && spec.better(score.unwrap(), *best, if best.is_some() { margin } else { 0.0 });
    if improved { *best = score; }
    let row = locked(&episode.dir, || {
        let row = Row { n: ledger(&episode.dir)?.len() + 1, episode: episode.label.clone(), lineage: episode.lineage,
            note: note.chars().take(300).collect(), snap, score, valid, se, improved, report: report.clone() };
        append_line(&episode.dir.join("ledger.jsonl"), &row)?;
        Ok(row)
    })?;
    Ok(json!({"n": row.n, "score": score, "valid": valid, "se": se, "improved": improved, "best": *best, "report": report}))
}

/// Run `f` holding the run's ledger lock, so episodes running side by side number their rows without gaps or repeats.
fn locked<T>(dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    use std::os::fd::AsRawFd;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("ledger.lock"))?;
    ensure!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0, "could not lock the ledger");
    let out = f();
    drop(lock);
    out
}

/// `ar.reset()`: make the episode's editable paths hold its lineage's best scored state again (when nothing has
/// improved it yet, the state the lineage started from).
pub fn reset(pol: &crate::policy::Policy) -> Result<Value> {
    let (episode, best) = EPISODE.get().context("ar.reset is only available inside an autoresearch episode")?;
    let rows = ledger(&episode.dir)?;
    let snap = rows.iter().rev().find(|r| r.improved && r.lineage == episode.lineage)
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
    Ok(json!({"best": *best.lock().unwrap()}))
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

/// Journal records. A lineage is recorded, with what sampling its direction cost, before any episode works on it. An
/// attempt is `started` before its child runs and `finished` after. Resume closes an attempt that never finished with
/// a durable `finished` record, charging what the child's journals in its leftover scratch directory recorded, or its
/// whole allowance when that directory is gone.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Lineage { id: u32, idea: Option<String>, start: String, cost: f64 },
    Started { attempt: usize, label: String, allowance: f64, #[serde(default)] scratch: Option<PathBuf>, #[serde(default)] lineage: u32 },
    Finished { attempt: usize, cost: f64, evals: usize, #[serde(default)] interrupted: bool, #[serde(default)] crashed: bool },
}

/// One line of search, as the journal and ledger tell it: a direction (none when sampling gave nothing) pursued from a
/// starting state. Ids start at 1; rows of ledgers older than lineages carry 0.
struct Lineage {
    id: u32,
    idea: Option<String>,
    start: String,
    /// The bar an eval must beat to improve the lineage, and the state its episodes start from: the last improvement,
    /// else the start. A direction gets its own hill, so its first valid score improves it even when that is worse than
    /// the state it started from; without a direction, the start's score is the bar.
    best: Option<f64>,
    best_snap: String,
    evals: usize,
    /// Episodes that scored something, and consecutive finished episodes that did not improve the lineage.
    tried: usize,
    idle: u32,
}

impl Lineage {
    fn active(&self) -> bool { self.idle < PATIENCE }
}

/// Everything the journal and ledger say about the search so far.
struct State {
    records: Vec<Record>,
    rows: Vec<Row>,
    lineages: Vec<Lineage>,
}

/// What one episode spent and did.
struct Outcome {
    cost: f64,
    evals: usize,
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

    fn seed(&self) -> Result<String> {
        Ok(std::fs::read_to_string(self.dir.join("seed"))?.trim().to_string())
    }

    fn state(&self) -> Result<State> {
        let records: Vec<Record> = read_lines(&self.dir.join("journal.jsonl"))?;
        let rows = ledger(&self.dir)?;
        let mut lineages: Vec<Lineage> = records.iter().filter_map(|r| {
            let Record::Lineage { id, idea, start, .. } = r else { return None };
            let own: Vec<&Row> = rows.iter().filter(|r| r.lineage == *id).collect();
            let last = own.iter().rev().find(|r| r.improved);
            Some(Lineage {
                id: *id, idea: idea.clone(), start: start.clone(),
                best: last.map_or_else(|| idea.is_none().then(|| self.known(&rows, start).0).flatten(), |r| r.score),
                best_snap: last.map_or_else(|| start.clone(), |r| r.snap.clone()),
                evals: own.len(), tried: 0, idle: 0,
            })
        }).collect();
        // Episode outcomes in journal order: an improvement resets a lineage's patience; a crash does not count.
        for r in &records {
            let Record::Finished { attempt, evals, crashed: false, .. } = r else { continue };
            let started = records.iter().find_map(|s| match s {
                Record::Started { attempt: a, label, lineage, .. } if a == attempt => Some((label, *lineage)),
                _ => None,
            });
            let Some((label, id)) = started else { continue };
            let Some(l) = lineages.iter_mut().find(|l| l.id == id) else { continue };
            if *evals > 0 { l.tried += 1; }
            if rows.iter().any(|r| r.episode == *label && r.improved) { l.idle = 0 } else { l.idle += 1 }
        }
        Ok(State { records, rows, lineages })
    }

    /// The lineage with the best score; ties and unscored lineages go to the earliest.
    fn leader<'a>(&self, lineages: impl Iterator<Item = &'a Lineage>) -> Option<&'a Lineage> {
        lineages.fold(None, |lead: Option<&Lineage>, l| match (lead, l.best) {
            (None, _) => Some(l),
            (Some(a), Some(b)) if self.spec().better(b, a.best, 0.0) => Some(l),
            (lead, _) => lead,
        })
    }

    /// Score the seed with the search evaluator before any episode, so directions are sampled with its report and
    /// episodes know where they start. Kept in `seed.json`, outside the ledger, which holds the agents' evaluations.
    async fn baseline(&self) -> Result<()> {
        let (score, valid, _, report) = evaluate(self.spec(), &self.dir, &load_snap(&self.dir, &self.seed()?)?, &self.spec().eval, false).await?;
        eprintln!("hx ar: seed scores {}", if valid { show(score) } else { "invalid".into() });
        std::fs::write(self.dir.join("seed.json"), serde_json::to_vec(&json!({"score": score.filter(|_| valid), "report": report}))?)?;
        Ok(())
    }

    /// The search score and report of `snap`: its first valid evaluation, or the host's baseline for the seed.
    fn known(&self, rows: &[Row], snap: &str) -> (Option<f64>, Value) {
        if let Some(r) = rows.iter().find(|r| r.snap == snap && r.valid) { return (r.score, r.report.clone()); }
        let baseline = (self.seed().ok().as_deref() == Some(snap)).then(|| std::fs::read(self.dir.join("seed.json")).ok()).flatten()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok()).unwrap_or(Value::Null);
        (baseline["score"].as_f64(), baseline["report"].clone())
    }

    /// Up to `n` deliberately different directions for a lineage starting from `start`, and what sampling them cost.
    /// Sampling never fails the search: without a provider answer there are just no directions.
    async fn ideate(&self, n: usize, start: &str, state: &State) -> (Vec<String>, f64) {
        let session = self.dir.file_name().unwrap_or_default().to_string_lossy().to_string();
        let (Ok(mut cfg), Ok(prompt)) = (crate::llm_cfg(&self.launch.child_args, session), self.ideation_prompt(start, state)) else { return (vec![], 0.0) };
        let (Ok(key), Ok(http)) = (cfg.key(), reqwest::Client::builder().timeout(std::time::Duration::from_secs(300)).build()) else { return (vec![], 0.0) };
        cfg.price = crate::llm::fetch_price(&cfg).await;
        let samples = futures::future::join_all((0..n).map(|_| crate::ssot::sample(&http, &cfg, &key, &prompt))).await;
        let cost = samples.iter().map(|s| s.1).sum();
        (samples.into_iter().filter_map(|s| s.0).collect(), cost)
    }

    fn ideation_prompt(&self, start: &str, state: &State) -> Result<String> {
        let mut t = format!("{}\n\n## Starting state\n", self.launch.program.trim_end());
        let mut left = STATE_CHARS;
        for (rel, bytes) in load_snap(&self.dir, start)? {
            let Ok(text) = String::from_utf8(bytes) else { continue };
            if left == 0 { break; }
            let shown = clip(&text, left);
            left = left.saturating_sub(text.chars().count());
            t += &format!("\n### {rel}\n\n```\n{shown}\n```\n");
        }
        let (score, report) = self.known(&state.rows, start);
        if score.is_some() {
            let report = serde_json::to_string(&report).unwrap_or_default();
            t += &format!("\nEvaluator report of this state (score {}):\n\n```json\n{}\n```\n", show(score), clip(&report, REPORT_CHARS / 2));
        }
        if !state.lineages.is_empty() {
            t += "\n## Directions already tried\n\n";
            for l in &state.lineages {
                t += &format!("- {} (best score {})\n", l.idea.as_deref().unwrap_or("(none: refine the current approach)").replace('\n', " "), show(l.best));
            }
        }
        t += &format!("\nPropose one research direction for this task: the core approach a solution should be built around, not a \
tweak, filter or parameter change on top of the starting state{}. Say what to build, why it could score well under this \
evaluator, and the first experiment. At most 120 words.", if state.lineages.is_empty() { "" } else { ", and different from every direction already tried" });
        Ok(t)
    }

    /// Start up to `n` lineages from `start`, each with its own sampled direction; one without a direction when
    /// sampling gives nothing.
    async fn spawn(&mut self, n: usize, start: &str, state: &State) -> Result<()> {
        let (ideas, cost) = self.ideate(n, start, state).await;
        let first = state.lineages.iter().map(|l| l.id).max().unwrap_or(0) + 1;
        let ideas: Vec<Option<String>> = if ideas.is_empty() { vec![None] } else { ideas.into_iter().map(Some).collect() };
        eprintln!("hx ar: {} new lineage(s) from state {start} (${cost:.4} to sample directions)", ideas.len());
        for (i, idea) in ideas.into_iter().enumerate() {
            self.journal(&Record::Lineage { id: first + i as u32, idea, start: start.into(), cost: if i == 0 { cost } else { 0.0 } })?;
        }
        self.spent += cost;
        Ok(())
    }

    fn task(&self, label: &str, allowance: f64, lineage: &Lineage, state: &State) -> Result<String> {
        let spec = self.spec();
        let better = if spec.direction == "max" { "higher" } else { "lower" };
        let mut t = format!("{}\n\n## Autoresearch episode {label}\n\n", self.launch.program.trim_end());
        t += &format!("You are one episode of an autoresearch loop. Improve the score by changing the editable paths: {}. \
Your workspace is a fresh copy of the project without Git history. You may create scratch files anywhere, but only the \
editable paths are scored and carried to later episodes.\n\n", spec.editable.join(", "));
        if let Some(idea) = &lineage.idea {
            let quoted: String = idea.lines().map(|l| format!("> {l}\n")).collect();
            t += &format!("## Direction\n\nThe search pursues several directions, each as its own lineage. This episode works on \
lineage {}, whose direction is:\n\n{quoted}\nBuild the solution around this direction: make it the core of the approach, not a filter \
or tweak on top of the current state, and give it a fair test before judging it. Its states are compared within this lineage, so \
early scores below other lineages are fine. If it clearly cannot work, say why in your ar.eval notes and finish.\n\n", lineage.id);
        }
        t += &format!("- `ar.eval(\"one-line hypothesis\")` scores the current editable files with the owner's frozen evaluator and returns \
{{n, score, valid, se, improved, best, report}}; {better} scores are better, and `improved` means better than this lineage's best. \
Only states scored by ar.eval count: anything you measure another way is discarded.\n\
- `ar` is a built-in global table like `sh` and `read`. `vars()` lists only your own globals, so it never shows `ar`: call `ar.eval` directly.\n\
- Every scored state is kept, and later episodes of this lineage start from its best one. `ar.reset()` puts this lineage's best \
state back into the editable paths: use it instead of reverting by hand. Bold changes are safe.\n\
- When the evaluator reports no `se`, it is deterministic: scoring a state it already scored validly returns the stored \
result with `cached: true`, so there is no need to re-check one.\n");
        if spec.min_delta > 0.0 {
            t += &format!("- A score counts as an improvement only when it beats the lineage's best by more than {}: smaller differences are noise.\n", spec.min_delta);
        }
        t += &format!("- Work in experiments: form a hypothesis, edit, ar.eval, read the report, decide the next step. One idea per eval, so the ledger stays readable.\n\
- This episode has about ${allowance:.2} of model budget. Use it on experiments. Finishing ends the episode; a later episode of this \
lineage continues from its best state with its ledger.\n");
        if spec.select.is_some() {
            t += "- The final result is chosen on held-out data you cannot see. Prefer changes with a reason to generalize over tuning \
to the reported numbers.\n";
        }
        t += "\n## Ledger of this lineage (oldest first)\n\n";
        let rows: Vec<&Row> = state.rows.iter().filter(|r| r.lineage == lineage.id).collect();
        t += if rows.is_empty() { "(empty: no evaluations yet)\n" } else { "n | score | improved | note\n" };
        let skip = rows.len().saturating_sub(SHOWN);
        if skip > 0 { t += &format!("({skip} older rows omitted)\n"); }
        for r in &rows[skip..] {
            let score = match (r.valid, r.score) { (true, Some(s)) => format!("{s:.6}"), _ => "invalid".into() };
            t += &format!("{} | {score} | {} | {}\n", r.n, if r.improved { "yes" } else { "" }, r.note.replace('\n', " "));
        }
        let others: Vec<&Lineage> = state.lineages.iter().filter(|l| l.id != lineage.id).collect();
        if !others.is_empty() {
            t += "\n## Other lineages\n\n";
            for l in others {
                let idea = clip(&l.idea.as_deref().unwrap_or("(no direction)").replace('\n', " "), 300);
                t += &format!("- lineage {}: {idea} | best {} over {} evals{}\n", l.id, show(l.best), l.evals, if l.active() { "" } else { ", retired" });
            }
        }
        let (score, report) = self.known(&state.rows, &lineage.best_snap);
        t += &match (lineage.best_snap == lineage.start, score) {
            (false, score) => format!("\nThe workspace holds this lineage's best state (score {}).\n", show(score)),
            (true, Some(score)) => format!("\nThe workspace holds this lineage's starting state (score {score:.6}), which it has not improved yet.\n"),
            (true, None) => "\nThe workspace holds this lineage's starting state; it has not been scored yet. Start with ar.eval on it as a baseline.\n".into(),
        };
        // The full report of that state, so an episode starts from the evaluator's detail rather than one number.
        if !report.is_null() {
            let text = serde_json::to_string_pretty(&report).unwrap_or_default();
            t += &format!("\n## Evaluator report of that state\n\n```json\n{}\n```\n", clip(&text, REPORT_CHARS));
        }
        Ok(t)
    }

    /// The search: lineages that have scored nothing yet run side by side, splitting what is left; then the leading
    /// active lineage gets the next episode; when every lineage is retired, a new direction starts from the best state.
    async fn search(&mut self) -> Result<()> {
        let (mut crashes, mut idle) = (0, 0);
        loop {
            let remaining = self.launch.budget - self.spent;
            if remaining < self.launch.budget * MIN_SLICE { break; }
            let state = self.state()?;
            let attempt = state.records.iter().filter_map(|r| match r { Record::Started { attempt, .. } => Some(*attempt), _ => None })
                .max().unwrap_or(0) + 1;
            if attempt > MAX_ATTEMPTS { break; }
            let fresh: Vec<&Lineage> = state.lineages.iter().filter(|l| l.tried == 0 && l.active()).collect();
            let batch = match self.leader(state.lineages.iter().filter(|l| l.active())) {
                _ if !fresh.is_empty() => fresh,
                Some(lead) => vec![lead],
                None => {
                    // The best state found: the leading lineage's, unless no lineage has beaten the seed.
                    let seed = self.seed()?;
                    let seed_score = self.known(&state.rows, &seed).0;
                    let start = match self.leader(state.lineages.iter()) {
                        Some(l) if l.best.is_some() && seed_score.is_none_or(|s| !self.spec().better(s, l.best, 0.0)) => l.best_snap.clone(),
                        _ => seed,
                    };
                    let n = if state.lineages.is_empty() { DRAFTS } else { 1 };
                    self.spawn(n, &start, &state).await?;
                    continue;
                }
            };
            eprintln!("hx ar: ${remaining:.2} left");
            let share = remaining / batch.len() as f64;
            let runs = batch.iter().enumerate().map(|(i, l)| self.episode(attempt + i, l, share, &state));
            let outcomes = futures::future::join_all(runs).await.into_iter().collect::<Result<Vec<_>>>()?;
            self.spent += outcomes.iter().map(|o| o.cost).sum::<f64>();
            if outcomes.iter().any(|o| o.evals > 0) { (crashes, idle) = (0, 0); continue; }
            if outcomes.iter().all(|o| o.crashed) {
                crashes += 1;
                // Before the first agent eval of the whole run, repeated crashes mean misconfiguration: stop, don't wait hours.
                if state.rows.is_empty() && crashes >= MAX_CRASHES {
                    let last: Vec<&str> = outcomes[0].stderr.trim().lines().rev().take(15).collect();
                    bail!("episode children keep failing before any eval; last error:\n{}", last.into_iter().rev().collect::<Vec<_>>().join("\n"));
                }
                if crashes > MAX_CRASHES { break; }
                let base = std::env::var("HX_AR_RETRY_SECS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(60);
                let pause = base << crashes;
                eprintln!("hx ar: episodes crashed before any eval; retrying in {pause}s");
                tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
            } else {
                // Agents that finish without scoring anything: one more round, then the search is over.
                idle += 1;
                if idle >= MAX_IDLE { break; }
            }
        }
        Ok(())
    }

    /// One fresh child run on `lineage`, in its own copy of the base holding the lineage's best state.
    async fn episode(&self, attempt: usize, lineage: &Lineage, allowance: f64, state: &State) -> Result<Outcome> {
        let tag = format!("e{attempt}");
        let ep_dir = self.dir.join("episodes");
        std::fs::create_dir_all(&ep_dir)?;
        let ep_path = ep_dir.join(format!("{tag}.json"));
        let episode = Episode { dir: self.dir.clone(), label: tag.clone(), spec: self.spec().clone(), lineage: lineage.id,
            start: lineage.start.clone(), best: lineage.best };
        std::fs::write(&ep_path, serde_json::to_vec_pretty(&episode)?)?;
        let task_path = ep_dir.join(format!("{tag}.md"));
        std::fs::write(&task_path, self.task(&tag, allowance, lineage, state)?)?;
        let scratch = Scratch::new("ws")?;
        let ws = scratch.0.join("ws");
        copy_tree(&self.dir.join("base"), &ws)?;
        place(&ws, self.spec(), &load_snap(&self.dir, &lineage.best_snap)?)?;
        self.journal(&Record::Started { attempt, label: tag.clone(), allowance, scratch: Some(scratch.0.clone()), lineage: lineage.id })?;
        eprintln!("hx ar: episode {tag} on lineage {} (${allowance:.2}, best {})", lineage.id, show(lineage.best));
        let out = tokio::process::Command::new(std::env::current_exe()?)
            .arg("run").arg("--workspace").arg(&ws).args(&self.launch.child_args)
            .arg("--prompt-file").arg(&task_path)
            .arg("--max-cost").arg(format!("{allowance:.6}"))
            .env("HX_AR_EPISODE", &ep_path)
            .stdin(std::process::Stdio::null())
            .output().await?;
        std::fs::write(ep_dir.join(format!("{tag}.stderr")), &out.stderr)?;
        let report = String::from_utf8_lossy(&out.stdout).lines().rev()
            .find_map(|l| serde_json::from_str::<Value>(l).ok()).unwrap_or(Value::Null);
        std::fs::write(ep_dir.join(format!("{tag}.report.json")), serde_json::to_vec_pretty(&report)?)?;
        let logs = keep_logs(&ws, &ep_dir, &tag)?;
        drop(scratch);
        let cost = report["cost_usd"].as_f64().unwrap_or_else(|| logged_cost(&logs, allowance));
        let evals = ledger(&self.dir)?.iter().filter(|r| r.episode == tag).count();
        let crashed = evals == 0 && report.is_null();
        self.journal(&Record::Finished { attempt, cost, evals, interrupted: false, crashed })?;
        let end = if report["finished"] == true { "finished" } else { report["stopped"].as_str().unwrap_or("stopped") };
        eprintln!("hx ar: episode {tag}: {evals} evals, ${cost:.3}, {end}");
        Ok(Outcome { cost, evals, crashed, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
    }

    /// The final pick: among the seed and every improvement of every lineage, the state that scores best on the select
    /// evaluator (held-out early stopping). Without select, or if select is unusable for all of them, the search best.
    async fn choose(&self, state: &State) -> Result<(String, Option<f64>, Option<f64>, bool, Vec<Value>)> {
        let seed = self.seed()?;
        let mut checkpoints: Vec<(String, Option<f64>)> = vec![(seed.clone(), self.known(&state.rows, &seed).0)];
        for r in state.rows.iter().filter(|r| r.improved) {
            if !checkpoints.iter().any(|c| c.0 == r.snap) { checkpoints.push((r.snap.clone(), r.score)); }
        }
        let search_best = checkpoints.iter().fold(checkpoints[0].clone(), |best, c| match c.1 {
            Some(s) if self.spec().better(s, best.1, 0.0) => c.clone(),
            _ => best,
        });
        let Some(cmd) = self.spec().select.clone() else { return Ok((search_best.0, search_best.1, None, false, vec![])) };
        let mut scored = vec![];
        let mut winner: Option<(String, Option<f64>, f64)> = None;
        for (snap, search) in checkpoints {
            let (score, valid, _, _) = evaluate(self.spec(), &self.dir, &load_snap(&self.dir, &snap)?, &cmd, true).await?;
            let held = score.filter(|_| valid);
            scored.push(json!({"snap": snap, "search": search, "select": held}));
            if let Some(h) = held && self.spec().better(h, winner.as_ref().map(|w| w.2), 0.0) { winner = Some((snap, search, h)); }
        }
        Ok(match winner {
            Some((snap, search, held)) => (snap, search, Some(held), false, scored),
            None => {
                eprintln!("hx ar: no checkpoint has a valid held-out score; keeping the search best");
                (search_best.0, search_best.1, None, true, scored)
            }
        })
    }

    async fn run(&mut self, mut records: Vec<Record>) -> Result<Value> {
        // Close attempts that never finished: charged their whole allowance, durably.
        let rows = ledger(&self.dir)?;
        let open: Vec<(usize, String, f64, Option<PathBuf>)> = records.iter().filter_map(|r| match r {
            Record::Started { attempt, label, allowance, scratch, .. }
                if !records.iter().any(|f| matches!(f, Record::Finished { attempt: a, .. } if a == attempt)) =>
                Some((*attempt, label.clone(), *allowance, scratch.clone())),
            _ => None,
        }).collect();
        for (attempt, label, allowance, scratch) in open {
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
            let record = Record::Finished { attempt, cost, evals, interrupted: true, crashed: evals == 0 };
            self.journal(&record)?;
            records.push(record);
        }
        self.spent += records.iter().map(|r| match r { Record::Finished { cost, .. } | Record::Lineage { cost, .. } => *cost, _ => 0.0 }).sum::<f64>();
        if !self.dir.join("seed.json").exists() { self.baseline().await?; }
        self.search().await?;
        let state = self.state()?;
        ensure!(!state.rows.is_empty(), "autoresearch made no evaluations; see {}", self.dir.join("episodes").display());
        let (snap, search, select, fallback, checkpoints) = self.choose(&state).await?;
        place(&self.workspace, self.spec(), &load_snap(&self.dir, &snap)?)?;
        let lineages: Vec<Value> = state.lineages.iter().map(|l| json!({"id": l.id, "idea": l.idea, "start": l.start,
            "best": l.best, "evals": l.evals, "episodes": l.tried, "retired": !l.active()})).collect();
        let report = json!({
            "run": self.dir.file_name().unwrap().to_string_lossy(), "dir": self.dir, "commit": self.launch.commit,
            "snap": snap, "search": search, "select": select, "fallback": fallback, "checkpoints": checkpoints,
            "lineages": lineages, "evals": state.rows.len(), "cost_usd": self.spent, "budget_usd": self.launch.budget,
        });
        std::fs::write(self.dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        Ok(report)
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
