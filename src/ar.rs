//! Autoresearch (`hx run --ar`): fork-and-flush search over a task's editable files, after Cai et al.,
//! "Fork-and-Flush: Escaping Idea Basins in Autoresearch Agents" (arXiv:2610.07447).
//!
//! The task supplies `.harness/ar.toml` (editable paths, evaluator commands, grader roots) and the program text
//! (`--prompt-file`). At launch the committed tree is frozen into the run's `base`. Agents never touch the owner's
//! workspace or its Git repository:
//!
//! - Every episode is a fresh child `hx run` (a flush) in a throwaway copy of `base` holding the lineage's best
//!   editable files. Only the editable paths ever leave an episode.
//! - `ar.eval` snapshots the editable files and scores the snapshot in a second throwaway copy of `base`, so the
//!   evaluator sees exactly the recorded bytes and nothing else the agent did.
//! - Evaluators run under their own sandbox: a scrubbed environment, no network, reads limited to the system,
//!   toolchains, the evaluation copy and the declared grader roots.
//! - The run directory (`.harness/ar/RUN`, with `base`, snapshots, ledger, journal and held-out scores) is sealed from
//!   agents in every mode.
//!
//! The best scored state of a lineage is host-owned, so agents never revert by hand. Fork rounds pick their winner
//! with the `select` evaluator, which agents never trigger.
use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Branches per fork round, fork rounds (at 0, ~1/3 and ~2/3 of the budget), and each branch's share of the budget.
const K: usize = 4;
const ROUNDS: usize = 3;
const EXPLORE_SHARE: f64 = 0.06;
/// Ledger rows shown to an episode, and characters of the best state's evaluator report.
const SHOWN: usize = 40;
const REPORT_CHARS: usize = 12_000;
/// Flushed restarts within one episode when the agent stops early (a guard; the allowance normally ends it first).
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
    /// Selection evaluator (held-out): used only by the orchestrator to pick fork winners and the final result.
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
    pub lineage: String,
    pub label: String,
    pub spec: Spec,
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
    lineage: String,
    episode: String,
    note: String,
    snap: String,
    score: Option<f64>,
    valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se: Option<f64>,
    improved: bool,
    /// The search evaluator's report, shown to later episodes for their lineage's best state.
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
    let (score, valid, se, report) = evaluate(spec, &episode.dir, &files, &spec.eval, false).await?;
    let mut best = best.lock().unwrap();
    let margin = spec.min_delta.max(se.unwrap_or(0.0));
    let improved = valid && spec.better(score.unwrap(), *best, if best.is_some() { margin } else { 0.0 });
    if improved { *best = score; }
    let n = ledger(&episode.dir)?.len() + 1;
    let row = Row { n, lineage: episode.lineage.clone(), episode: episode.label.clone(), note: note.chars().take(300).collect(),
        snap, score, valid, se, improved, report: report.clone() };
    append_line(&episode.dir.join("ledger.jsonl"), &row)?;
    Ok(json!({"n": n, "score": score, "valid": valid, "se": se, "improved": improved, "best": *best, "report": report}))
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

/// Where a lineage forks from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum Origin { Seed, Winner(usize) }

/// One step of the fixed schedule. The plan is a pure function of the schedule name, so a journal of completed
/// steps is all a resumed run needs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum Step {
    Fork { id: String, from: Origin },
    /// `lineage: None` continues the winner of `round`.
    Episode { lineage: Option<String>, round: usize, label: String, share: f64 },
    /// Pick the best of `candidates` (or of every round winner when empty) on the select evaluator.
    Choose { round: usize, candidates: Vec<String> },
}

/// `forkflush`: the paper's schedule. `single`: one lineage with the whole budget, the baseline it is compared to.
fn plan(schedule: &str) -> Result<Vec<Step>> {
    let mut steps = vec![];
    match schedule {
        "forkflush" => {
            let exploit = (1.0 - EXPLORE_SHARE * (K * ROUNDS) as f64) / ROUNDS as f64;
            for round in 0..ROUNDS {
                let from = if round == 0 { Origin::Seed } else { Origin::Winner(round - 1) };
                let ids: Vec<String> = (0..K).map(|k| format!("r{round}b{k}")).collect();
                for id in &ids {
                    steps.push(Step::Fork { id: id.clone(), from: from.clone() });
                    steps.push(Step::Episode { lineage: Some(id.clone()), round, label: id.clone(), share: EXPLORE_SHARE });
                }
                steps.push(Step::Choose { round, candidates: ids });
                steps.push(Step::Episode { lineage: None, round, label: format!("r{round}lead"), share: exploit });
            }
            // Leaders keep improving after their round's selection, so the round winners compete once more.
            steps.push(Step::Choose { round: ROUNDS, candidates: vec![] });
        }
        "single" => {
            steps.push(Step::Fork { id: "s0".into(), from: Origin::Seed });
            steps.push(Step::Episode { lineage: Some("s0".into()), round: 0, label: "s0".into(), share: 1.0 });
            steps.push(Step::Choose { round: 0, candidates: vec!["s0".into()] });
        }
        _ => bail!("unknown autoresearch schedule {schedule:?} (forkflush or single)"),
    }
    Ok(steps)
}

/// Launch settings, frozen at the start and reused on resume.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Launch {
    spec: Spec,
    program: String,
    child_args: Vec<String>,
    budget: f64,
    schedule: String,
    commit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Lineage {
    id: String,
    /// The lineage it forked from, whose ledger rows up to `fork_at` it inherits.
    parent: Option<String>,
    fork_at: usize,
    start: String,
    start_score: Option<f64>,
}

/// Journal records: an attempt is `started` before its child runs and `finished` after. Resume closes an attempt
/// that never finished with a `finished` record charging its whole allowance, so the charge survives later resumes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Forked { step: usize, lineage: Lineage },
    Started { step: usize, attempt: usize, label: String, allowance: f64 },
    Finished { step: usize, attempt: usize, cost: f64, evals: usize, #[serde(default)] interrupted: bool },
    EpisodeDone { step: usize },
    Chosen { step: usize, round: usize, winner: String, candidates: Vec<Value>, #[serde(default)] fallback: bool },
}

struct Orchestrator {
    workspace: PathBuf,
    dir: PathBuf,
    launch: Launch,
    spent: f64,
    lineages: Vec<Lineage>,
    winners: BTreeMap<usize, String>,
    chosen: Vec<Value>,
}

impl Orchestrator {
    fn spec(&self) -> &Spec { &self.launch.spec }

    fn journal(&self, record: &Record) -> Result<()> {
        append_line(&self.dir.join("journal.jsonl"), record)
    }

    fn lineage(&self, id: &str) -> &Lineage {
        self.lineages.iter().find(|l| l.id == id).expect("lineage forked before use")
    }

    /// Rows of lineage `id` and its ancestors up to each fork point, in ledger order.
    fn history(&self, id: &str, rows: &[Row]) -> Vec<Row> {
        let mut chain = vec![(id.to_string(), usize::MAX)];
        let (mut at, mut limit) = (self.lineage(id), usize::MAX);
        while let Some(parent) = &at.parent {
            limit = limit.min(at.fork_at);
            chain.push((parent.clone(), limit));
            at = self.lineage(parent);
        }
        rows.iter().filter(|r| chain.iter().any(|(l, lim)| *l == r.lineage && r.n <= *lim)).cloned().collect()
    }

    /// Best scored state of a lineage: its last improvement, or the state it started from.
    fn best(&self, id: &str, rows: &[Row]) -> (String, Option<f64>) {
        let l = self.lineage(id);
        let mut best = (l.start.clone(), l.start_score);
        for r in rows.iter().filter(|r| r.lineage == l.id && r.improved) {
            best = (r.snap.clone(), r.score);
        }
        best
    }

    fn task(&self, id: &str, label: &str, allowance: f64, rows: &[Row]) -> String {
        let spec = self.spec();
        let history = self.history(id, rows);
        let (best_snap, best) = self.best(id, rows);
        let better = if spec.direction == "max" { "higher" } else { "lower" };
        let mut t = format!("{}\n\n## Autoresearch episode {label}\n\n", self.launch.program.trim_end());
        t += &format!("You are one episode of an autoresearch loop. Improve the score by changing the editable paths: {}. \
Your workspace is a fresh copy of the project without Git history. You may create scratch files anywhere, but only the \
editable paths are scored and carried to later episodes.\n\n", spec.editable.join(", "));
        t += &format!("- `ar.eval(\"one-line hypothesis\")` scores the current editable files with the owner's frozen evaluator and returns \
{{n, score, valid, improved, best, report}}; {better} scores are better. Only states scored by ar.eval count.\n\
- The host keeps the best scored state of this line of work automatically. Bold changes are safe: you never need to revert by hand.\n");
        if spec.min_delta > 0.0 {
            t += &format!("- A score counts as an improvement only when it beats the best by more than {}: smaller differences are noise.\n", spec.min_delta);
        }
        t += &format!("- Work in experiments: form a hypothesis, edit, ar.eval, read the report, decide the next step. One idea per eval, so the ledger stays readable.\n\
- This episode has about ${allowance:.2} of model budget. Use it on experiments. Finishing ends the episode; when you stop early, a fresh \
agent continues from the best state with this ledger.\n");
        if spec.select.is_some() {
            t += "- The final result is chosen on held-out data you cannot see. Prefer changes with a reason to generalize over tuning \
to the reported numbers.\n";
        }
        t += "\n## Ledger of this line of work (oldest first)\n\n";
        if history.is_empty() {
            t += "(empty: no evaluations yet)\n";
        } else {
            t += "n | score | improved | note\n";
            let skip = history.len().saturating_sub(SHOWN);
            if skip > 0 { t += &format!("({skip} older rows omitted)\n"); }
            for r in &history[skip..] {
                let score = match (r.valid, r.score) { (true, Some(s)) => format!("{s:.6}"), _ => "invalid".into() };
                t += &format!("{} | {score} | {} | {}\n", r.n, if r.improved { "yes" } else { "" }, r.note.replace('\n', " "));
            }
        }
        t += &match best {
            Some(b) => format!("\nThe workspace holds the best state so far (score {b:.6}).\n"),
            None => "\nThe workspace holds the starting state; it has not been scored yet. Start with ar.eval on it as a baseline.\n".into(),
        };
        // The full report of that state, so an episode starts from the evaluator's detail rather than one number.
        if let Some(r) = history.iter().rev().find(|r| r.snap == best_snap && r.valid && !r.report.is_null()) {
            let text = serde_json::to_string_pretty(&r.report).unwrap_or_default();
            let cut: String = text.chars().take(REPORT_CHARS).collect();
            t += &format!("\n## Evaluator report of the best state (eval {})\n\n```json\n{cut}{}\n```\n", r.n,
                if cut.len() < text.len() { "\n... (truncated)" } else { "" });
        }
        t
    }

    fn fork(&mut self, step: usize, id: &str, from: &Origin) -> Result<()> {
        let rows = ledger(&self.dir)?;
        let (parent, start, start_score) = match from {
            Origin::Seed => (None, self.seed()?, None),
            Origin::Winner(round) => {
                let parent = self.winners[round].clone();
                let (snap, score) = self.best(&parent, &rows);
                (Some(parent), snap, score)
            }
        };
        let lineage = Lineage { id: id.into(), parent, fork_at: rows.len(), start, start_score };
        self.journal(&Record::Forked { step, lineage: lineage.clone() })?;
        self.lineages.push(lineage);
        Ok(())
    }

    fn seed(&self) -> Result<String> {
        Ok(std::fs::read_to_string(self.dir.join("seed"))?.trim().to_string())
    }

    /// Fresh child runs on lineage `id`, each in its own copy of the base, until `allowance` is spent or an attempt
    /// makes no eval. `used` and `first` carry what earlier (possibly interrupted) attempts of this step consumed.
    async fn episode(&mut self, step: usize, id: &str, label: &str, allowance: f64, mut used: f64, first: usize) -> Result<()> {
        let mut crashes = 0;
        for attempt in first..=MAX_ATTEMPTS {
            let remaining = (allowance - used).min(self.launch.budget - self.spent);
            // Continue while a meaningful slice is left: 15% of a branch allowance, whatever the schedule, so every
            // schedule spends its budget equally fully.
            if remaining < allowance.min(self.launch.budget * EXPLORE_SHARE) * 0.15 || remaining <= 0.0 { break; }
            let rows = ledger(&self.dir)?;
            let (snap, best) = self.best(id, &rows);
            let tag = format!("{label}.{attempt}");
            let ep_dir = self.dir.join("episodes");
            std::fs::create_dir_all(&ep_dir)?;
            let ep_path = ep_dir.join(format!("{tag}.json"));
            let episode = Episode { dir: self.dir.clone(), lineage: id.into(), label: tag.clone(), spec: self.spec().clone(), best };
            std::fs::write(&ep_path, serde_json::to_vec_pretty(&episode)?)?;
            let task_path = ep_dir.join(format!("{tag}.md"));
            std::fs::write(&task_path, self.task(id, &tag, remaining, &rows))?;
            let scratch = Scratch::new("ws")?;
            let ws = scratch.0.join("ws");
            copy_tree(&self.dir.join("base"), &ws)?;
            place(&ws, self.spec(), &load_snap(&self.dir, &snap)?)?;
            self.journal(&Record::Started { step, attempt, label: tag.clone(), allowance: remaining })?;
            eprintln!("hx ar: episode {tag} (lineage {id}, ${remaining:.2}, best {})", show(best));
            let out = tokio::process::Command::new(std::env::current_exe()?)
                .arg("run").arg("--workspace").arg(&ws).args(&self.launch.child_args)
                .arg("--prompt-file").arg(&task_path)
                .arg("--max-cost").arg(format!("{remaining:.6}"))
                .env("HX_AR_EPISODE", &ep_path)
                .stdin(std::process::Stdio::null())
                .output().await?;
            std::fs::write(ep_dir.join(format!("{tag}.stderr")), &out.stderr)?;
            let report = String::from_utf8_lossy(&out.stdout).lines().rev()
                .find_map(|l| serde_json::from_str::<Value>(l).ok()).unwrap_or(Value::Null);
            std::fs::write(ep_dir.join(format!("{tag}.report.json")), serde_json::to_vec_pretty(&report)?)?;
            // Keep the child's journals; the rest of its workspace is discarded with the scratch directory.
            let logs = ep_dir.join(format!("{tag}.log"));
            if ws.join(".harness/log").is_dir() { copy_tree(&ws.join(".harness/log"), &logs)?; }
            drop(scratch);
            // A child that died without a report is charged what its journals recorded, unsettled requests at their
            // reservation; with no journal at all, its whole allowance.
            let cost = match report["cost_usd"].as_f64() {
                Some(cost) => cost,
                None => {
                    let journals: Vec<PathBuf> = std::fs::read_dir(&logs).into_iter().flatten().flatten().map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
                    if journals.is_empty() { remaining } else { journals.iter().map(|j| journal_cost(j).unwrap_or(remaining)).sum() }
                }
            };
            let all = ledger(&self.dir)?;
            let evals = all.iter().filter(|r| r.episode == tag).count();
            self.journal(&Record::Finished { step, attempt, cost, evals, interrupted: false })?;
            used += cost;
            self.spent += cost;
            let end = if report["finished"] == true { "finished" } else { report["stopped"].as_str().unwrap_or("stopped") };
            eprintln!("hx ar: episode {tag}: {evals} evals, ${cost:.3}, {end}");
            if evals == 0 {
                if report.is_null() {
                    crashes += 1;
                    // Before the first eval of the whole run, repeated crashes mean misconfiguration: stop, don't wait hours.
                    if all.is_empty() && crashes >= MAX_CRASHES {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let last: Vec<&str> = stderr.trim().lines().rev().take(15).collect();
                        bail!("episode children keep failing before any eval; last error:\n{}", last.into_iter().rev().collect::<Vec<_>>().join("\n"));
                    }
                    if crashes <= MAX_CRASHES {
                        let base = std::env::var("HX_AR_RETRY_SECS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(60);
                        let pause = base << crashes;
                        eprintln!("hx ar: episode {tag} crashed before any eval; retrying in {pause}s");
                        tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
                        continue;
                    }
                }
                break;
            }
        }
        self.journal(&Record::EpisodeDone { step })
    }

    /// Pick the lineage whose best state scores best on the select evaluator. If select is unusable for every
    /// candidate, fall back to the search score and say so.
    async fn choose(&mut self, step: usize, round: usize, candidates: &[String]) -> Result<()> {
        let rows = ledger(&self.dir)?;
        let mut scored: Vec<(String, String, Option<f64>, Option<f64>)> = vec![];
        for id in candidates {
            let (snap, search) = self.best(id, &rows);
            let held = match (&self.spec().select, search) {
                (Some(cmd), Some(_)) => {
                    let (score, valid, _, _) = evaluate(self.spec(), &self.dir, &load_snap(&self.dir, &snap)?, cmd, true).await?;
                    score.filter(|_| valid)
                }
                (None, s) => s,
                (Some(_), None) => None,
            };
            scored.push((id.clone(), snap, search, held));
        }
        let pick = |held: bool| {
            let mut winner: Option<(&String, f64)> = None;
            for (id, _, search, select) in &scored {
                if let Some(v) = if held { *select } else { *search } && self.spec().better(v, winner.map(|w| w.1), 0.0) {
                    winner = Some((id, v));
                }
            }
            winner.map(|w| w.0.clone())
        };
        let (winner, fallback) = match pick(true) {
            Some(w) => (w, false),
            None => {
                eprintln!("hx ar: round {round}: no candidate has a valid held-out score; choosing on the search score");
                (pick(false).unwrap_or_else(|| candidates[0].clone()), true)
            }
        };
        eprintln!("hx ar: round {round} → {winner}");
        let candidates = scored.iter().map(|(l, s, search, held)| json!({"lineage": l, "snap": s, "search": search, "select": held})).collect();
        let record = Record::Chosen { step, round, winner, candidates, fallback };
        self.journal(&record)?;
        self.apply(&record);
        Ok(())
    }

    fn apply(&mut self, record: &Record) {
        match record {
            Record::Forked { lineage, .. } => self.lineages.push(lineage.clone()),
            Record::Finished { cost, .. } => self.spent += cost,
            Record::Chosen { round, winner, candidates, fallback, .. } => {
                self.winners.insert(*round, winner.clone());
                self.chosen.push(json!({"round": round, "winner": winner, "candidates": candidates, "fallback": fallback}));
            }
            Record::Started { .. } | Record::EpisodeDone { .. } => {}
        }
    }

    async fn run(&mut self, mut records: Vec<Record>) -> Result<Value> {
        // Close attempts that never finished: charged their whole allowance, durably.
        let rows = ledger(&self.dir)?;
        let open: Vec<(usize, usize, String, f64)> = records.iter().filter_map(|r| match r {
            Record::Started { step, attempt, label, allowance }
                if !records.iter().any(|f| matches!(f, Record::Finished { step: s, attempt: a, .. } if s == step && a == attempt)) =>
                Some((*step, *attempt, label.clone(), *allowance)),
            _ => None,
        }).collect();
        for (step, attempt, label, allowance) in open {
            let evals = rows.iter().filter(|r| r.episode == label).count();
            let record = Record::Finished { step, attempt, cost: allowance, evals, interrupted: true };
            self.journal(&record)?;
            records.push(record);
        }
        for record in &records { self.apply(record); }
        let steps = plan(&self.launch.schedule)?;
        let done = |i: usize| records.iter().any(|r| matches!(r, Record::Forked { step, .. } | Record::EpisodeDone { step } | Record::Chosen { step, .. } if *step == i));
        for (i, step) in steps.iter().enumerate() {
            if done(i) { continue; }
            match step {
                Step::Fork { id, from } => self.fork(i, id, from)?,
                Step::Episode { lineage, round, label, share } => {
                    let id = lineage.clone().unwrap_or_else(|| self.winners[round].clone());
                    let (mut used, mut next) = (0.0, 1);
                    for r in &records {
                        if let Record::Finished { step, attempt, cost, .. } = r && *step == i {
                            used += cost;
                            next = next.max(attempt + 1);
                        }
                    }
                    self.episode(i, &id, label, self.launch.budget * share, used, next).await?;
                }
                Step::Choose { round, candidates } => {
                    let candidates = if candidates.is_empty() {
                        let mut all: Vec<String> = self.winners.values().cloned().collect();
                        all.dedup();
                        all
                    } else { candidates.clone() };
                    self.choose(i, *round, &candidates).await?;
                }
            }
        }
        let rows = ledger(&self.dir)?;
        ensure!(!rows.is_empty(), "autoresearch made no evaluations; see {}", self.dir.join("episodes").display());
        let last = self.chosen.last().cloned().context("the schedule ends with a selection")?;
        let winner = last["winner"].as_str().unwrap().to_string();
        let (snap, search) = self.best(&winner, &rows);
        place(&self.workspace, self.spec(), &load_snap(&self.dir, &snap)?)?;
        let select = last["candidates"].as_array().and_then(|c| c.iter().find(|c| c["lineage"] == winner.as_str())).map(|c| c["select"].clone());
        let report = json!({
            "run": self.dir.file_name().unwrap().to_string_lossy(), "dir": self.dir, "schedule": self.launch.schedule,
            "commit": self.launch.commit, "winner": winner, "snap": snap, "search": search, "select": select,
            "evals": rows.len(), "cost_usd": self.spent, "budget_usd": self.launch.budget, "selections": self.chosen,
        });
        std::fs::write(self.dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        Ok(report)
    }
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

/// `hx run --ar`: run the schedule over the whole `--max-cost` budget, or continue the run in `resume` (a run directory).
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
            let schedule = std::env::var("HX_AR_SCHEDULE").unwrap_or_else(|_| "forkflush".into());
            plan(&schedule)?;
            let bad = uncommitted(workspace, &spec)?;
            ensure!(bad.is_empty(), "--ar freezes the committed tree: commit or remove changes outside the editable paths: {}", bad.join(", "));
            let id = format!("{}-{}", chrono_stamp(), std::process::id());
            let dir = workspace.join(".harness/ar").join(&id);
            std::fs::create_dir_all(&dir)?;
            let commit = freeze(workspace, &dir)?;
            std::fs::write(dir.join("seed"), save_snap(&dir, &capture(workspace, &spec)?)?)?;
            let launch = Launch { spec, program, child_args, budget, schedule, commit };
            std::fs::write(dir.join("launch.json"), serde_json::to_vec_pretty(&launch)?)?;
            (dir, launch, vec![])
        }
        (None, None) => bail!("--ar needs a program and budget, or --resume RUN_DIR"),
    };
    let budget = launch.budget;
    let mut o = Orchestrator { workspace: workspace.to_path_buf(), dir, launch, spent: 0.0, lineages: vec![], winners: BTreeMap::new(), chosen: vec![] };
    eprintln!("hx ar: run {} ({}, ${budget:.2}){}", o.dir.display(), o.launch.schedule, if records.is_empty() { "" } else { ", resumed" });
    let report = o.run(records).await?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}
