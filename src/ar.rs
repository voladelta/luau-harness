//! Autoresearch (`hx run --ar`): fork-and-flush search over a task's editable files, after Cai et al.,
//! "Fork-and-Flush: Escaping Idea Basins in Autoresearch Agents" (arXiv:2610.07447).
//!
//! The task supplies `.harness/ar.toml` (editable paths and evaluator commands) and the program text (`--prompt-file`).
//! The orchestrator runs every episode as a fresh child `hx run` (a flush: new conversation, workspace kept). A child
//! can score candidates only through the host's `ar.eval`, which checks that nothing outside the editable paths
//! changed, snapshots the editable files, runs the frozen search evaluator and appends to the ledger. The best scored
//! state of a lineage is host-owned, so agents never revert by hand. Fork rounds pick their winner with the `select`
//! evaluator, which agents never see.
//!
//! Evaluators run as the owner's commands outside the agent sandbox. Keep the evaluator and its data outside the
//! workspace: agents can write anything there, including ignored files such as build outputs.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Branches per fork round, fork rounds (at 0, ~1/3 and ~2/3 of the budget), and each branch's share of the budget.
const K: usize = 4;
const ROUNDS: usize = 3;
const EXPLORE_SHARE: f64 = 0.06;
/// Ledger rows shown to an episode.
const SHOWN: usize = 40;
/// Flushed restarts within one episode when the agent stops early.
const MAX_ATTEMPTS: usize = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Files or directories the agent may change; everything else tracked by Git must stay as committed.
    pub editable: Vec<String>,
    /// Search evaluator: agent-visible. Prints a JSON object with `score` (and optionally `valid`, `se`, `report`)
    /// as its last stdout line.
    pub eval: String,
    /// Selection evaluator (held-out): used only by the orchestrator to pick fork winners and the final result.
    pub select: Option<String>,
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
            ensure!(!p.is_empty() && !Path::new(p).is_absolute() && !p.split('/').any(|c| c == ".." || c == ".harness"),
                "ar.toml: editable path {p:?} must be relative, inside the workspace and outside .harness");
        }
        ensure!(matches!(spec.direction.as_str(), "max" | "min"), "ar.toml: direction must be max or min");
        ensure!(spec.timeout > 0.0 && spec.min_delta >= 0.0, "ar.toml: timeout must be positive and min_delta nonnegative");
        Ok(spec)
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
    pub dir: PathBuf,
    pub lineage: String,
    pub label: String,
    pub spec: Spec,
    pub best: Option<f64>,
}
pub static EPISODE: OnceLock<(Episode, Mutex<Option<f64>>)> = OnceLock::new();

/// Called once at startup: a child run of an autoresearch episode gets `ar.eval`.
pub fn load_episode() -> Result<()> {
    if let Some(path) = std::env::var_os("HX_AR_EPISODE") {
        let episode: Episode = serde_json::from_slice(&std::fs::read(&path)?).context("HX_AR_EPISODE")?;
        let best = episode.best;
        let _ = EPISODE.set((episode, Mutex::new(best)));
    }
    Ok(())
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
}

fn ledger(dir: &Path) -> Result<Vec<Row>> {
    let Ok(text) = std::fs::read_to_string(dir.join("ledger.jsonl")) else { return Ok(vec![]) };
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| Ok(serde_json::from_str(l)?)).collect()
}

fn git(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = std::process::Command::new("git").args(args).current_dir(workspace).output().context("git")?;
    ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

fn editable(spec: &Spec, path: &str) -> bool {
    spec.editable.iter().any(|e| path == e || path.starts_with(&format!("{}/", e.trim_end_matches('/'))))
}

/// Changed or untracked paths outside the editable set (ignored files and hx's own state excluded).
fn out_of_scope(workspace: &Path, spec: &Spec) -> Result<Vec<String>> {
    let out = git(workspace, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let mut bad = vec![];
    let mut entries = out.split(|b| *b == 0).filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let entry = String::from_utf8_lossy(entry);
        let (status, path) = entry.split_at(3.min(entry.len()));
        // A rename's source path follows as its own entry.
        if status.starts_with('R') || status.starts_with('C') { entries.next(); }
        if path.starts_with(".harness/") || path.starts_with(".agents/") || editable(spec, path) { continue; }
        bad.push(path.to_string());
    }
    Ok(bad)
}

/// Every file under the editable paths, as UTF-8 text (a missing path is simply absent).
fn capture(workspace: &Path, spec: &Spec) -> Result<BTreeMap<String, String>> {
    fn walk(path: &Path, rel: &str, out: &mut BTreeMap<String, String>) -> Result<()> {
        let meta = match std::fs::symlink_metadata(path) { Ok(m) => m, Err(_) => return Ok(()) };
        ensure!(!meta.file_type().is_symlink(), "editable path {rel} is a symlink");
        if meta.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                walk(&entry.path(), &format!("{rel}/{name}"), out)?;
            }
        } else {
            let text = String::from_utf8(std::fs::read(path)?).map_err(|_| anyhow::anyhow!("editable file {rel} is not UTF-8"))?;
            out.insert(rel.to_string(), text);
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    for e in &spec.editable {
        let e = e.trim_end_matches('/');
        walk(&workspace.join(e), e, &mut out)?;
    }
    Ok(out)
}

/// Store a snapshot content-addressed under `dir/snaps`; returns its id.
fn save_snap(dir: &Path, files: &BTreeMap<String, String>) -> Result<String> {
    let bytes = serde_json::to_vec(files)?;
    let id = crate::eventlog::sha_bytes(&bytes)[..16].to_string();
    let path = dir.join("snaps").join(format!("{id}.json"));
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, bytes)?;
    }
    Ok(id)
}

/// Make the editable paths hold exactly snapshot `id`.
fn restore(workspace: &Path, spec: &Spec, dir: &Path, id: &str) -> Result<()> {
    let files: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(dir.join("snaps").join(format!("{id}.json")))?)?;
    for stale in capture(workspace, spec)?.keys().filter(|k| !files.contains_key(*k)) {
        std::fs::remove_file(workspace.join(stale))?;
    }
    for (rel, text) in &files {
        let path = workspace.join(rel);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text) {
            if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
            std::fs::write(path, text)?;
        }
    }
    Ok(())
}

/// Run an evaluator command; returns (score, valid, se, report). Failures become invalid results, not errors.
/// Evaluators are the owner's trusted commands (`ar.toml` is covered by `hx trust`) and run outside the agent sandbox,
/// so the grader and its data can live where agents can neither read nor write them.
async fn evaluate(pol: &crate::policy::Policy, spec: &Spec, cmd: &str) -> Result<(Option<f64>, bool, Option<f64>, Value)> {
    let (stdout, stderr, code, timed_out) = crate::prims::shell(None, cmd, pol.workspace.clone(), spec.timeout).await
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

/// `ar.eval(note)` inside an episode: check scope, snapshot, score with the search evaluator, append to the ledger.
pub async fn eval(pol: &crate::policy::Policy, note: String) -> Result<Value> {
    let (episode, best) = EPISODE.get().context("ar.eval is only available inside an autoresearch episode")?;
    let spec = &episode.spec;
    let bad = out_of_scope(&pol.workspace, spec)?;
    if !bad.is_empty() {
        bail!("only the editable paths {:?} may differ from Git HEAD; revert or delete: {} (scratch files belong in tmpdir())",
            spec.editable, bad.join(", "));
    }
    let snap = save_snap(&episode.dir, &capture(&pol.workspace, spec)?)?;
    let (score, valid, se, report) = evaluate(pol, spec, &spec.eval).await?;
    let mut best = best.lock().unwrap();
    let margin = spec.min_delta.max(se.unwrap_or(0.0));
    let improved = valid && spec.better(score.unwrap(), *best, if best.is_some() { margin } else { 0.0 });
    if improved { *best = score; }
    let n = ledger(&episode.dir)?.len() + 1;
    let row = Row { n, lineage: episode.lineage.clone(), episode: episode.label.clone(), note: note.chars().take(300).collect(),
        snap, score, valid, se, improved };
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(episode.dir.join("ledger.jsonl"))?;
    writeln!(file, "{}", serde_json::to_string(&row)?)?;
    file.sync_all()?;
    Ok(json!({"n": n, "score": score, "valid": valid, "se": se, "improved": improved, "best": *best, "report": report}))
}

struct Lineage {
    id: String,
    parent: Option<usize>,
    /// Ledger rows of the parent visible to this lineage: n <= fork_at.
    fork_at: usize,
    start: String,
    start_score: Option<f64>,
}

struct Orchestrator {
    workspace: PathBuf,
    dir: PathBuf,
    spec: Spec,
    program: String,
    child_args: Vec<String>,
    budget: f64,
    spent: f64,
    lineages: Vec<Lineage>,
    selections: Vec<Value>,
    pol: std::sync::Arc<crate::policy::Policy>,
}

impl Orchestrator {
    /// Rows of lineage `i` and its ancestors up to each fork point, in ledger order.
    fn history(&self, i: usize, rows: &[Row]) -> Vec<Row> {
        let mut chain = vec![(i, usize::MAX)];
        let (mut at, mut limit) = (i, usize::MAX);
        while let Some(p) = self.lineages[at].parent {
            limit = limit.min(self.lineages[at].fork_at);
            chain.push((p, limit));
            at = p;
        }
        rows.iter().filter(|r| chain.iter().any(|(l, lim)| self.lineages[*l].id == r.lineage && r.n <= *lim)).cloned().collect()
    }

    /// Best scored state of lineage `i` (its own valid rows, or the state it started from).
    fn best(&self, i: usize, rows: &[Row]) -> (String, Option<f64>) {
        let l = &self.lineages[i];
        let mut best = (l.start.clone(), l.start_score);
        for r in rows.iter().filter(|r| r.lineage == l.id && r.valid) {
            if self.spec.better(r.score.unwrap(), best.1, 0.0) { best = (r.snap.clone(), r.score); }
        }
        best
    }

    fn fork(&mut self, parent: Option<usize>, id: String) -> Result<usize> {
        let rows = ledger(&self.dir)?;
        let (start, start_score) = match parent {
            Some(p) => self.best(p, &rows),
            None => (save_snap(&self.dir, &capture(&self.workspace, &self.spec)?)?, None),
        };
        self.lineages.push(Lineage { id, parent, fork_at: rows.len(), start, start_score });
        Ok(self.lineages.len() - 1)
    }

    fn task(&self, i: usize, label: &str, allowance: f64, rows: &[Row]) -> String {
        let history = self.history(i, rows);
        let (_, best) = self.best(i, rows);
        let better = if self.spec.direction == "max" { "higher" } else { "lower" };
        let mut t = format!("{}\n\n## Autoresearch episode {label}\n\n", self.program.trim_end());
        t += &format!("You are one episode of an autoresearch loop. Improve the score by changing only the editable paths: {}. \
Every other file tracked by Git must stay as committed; put scratch files in tmpdir().\n\n", self.spec.editable.join(", "));
        t += &format!("- `ar.eval(\"one-line hypothesis\")` scores the current editable files with the owner's frozen evaluator and returns \
{{n, score, valid, improved, best, report}}; {better} scores are better. Only states scored by ar.eval count.\n\
- The host keeps the best scored state of this line of work automatically. Bold changes are safe: you never need to revert by hand.\n\
- Work in experiments: form a hypothesis, edit, ar.eval, read the report, decide the next step. One idea per eval, so the ledger stays readable.\n\
- This episode has about ${allowance:.2} of model budget. Use it on experiments. Finishing ends the episode; when you stop early, a fresh \
agent continues from the best state with this ledger.\n");
        if self.spec.select.is_some() {
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
        t
    }

    /// Run episode `label` of lineage `i` with `allowance` dollars: fresh child runs until it is spent or no progress is possible.
    async fn episode(&mut self, i: usize, label: &str, allowance: f64) -> Result<()> {
        let mut used = 0.0;
        for attempt in 1..=MAX_ATTEMPTS {
            let remaining = (allowance - used).min(self.budget - self.spent);
            if remaining < allowance * 0.15 || remaining <= 0.0 { break; }
            let rows = ledger(&self.dir)?;
            let (snap, best) = self.best(i, &rows);
            restore(&self.workspace, &self.spec, &self.dir, &snap)?;
            let tag = format!("{label}.{attempt}");
            let episode = Episode { dir: self.dir.clone(), lineage: self.lineages[i].id.clone(), label: tag.clone(), spec: self.spec.clone(), best };
            let ep_dir = self.dir.join("episodes");
            std::fs::create_dir_all(&ep_dir)?;
            let ep_path = ep_dir.join(format!("{tag}.json"));
            std::fs::write(&ep_path, serde_json::to_vec_pretty(&episode)?)?;
            let task_path = ep_dir.join(format!("{tag}.md"));
            std::fs::write(&task_path, self.task(i, &tag, remaining, &rows))?;
            let before = rows.len();
            eprintln!("hx ar: episode {tag} (lineage {}, ${remaining:.2}, best {})", self.lineages[i].id, show(best));
            let out = tokio::process::Command::new(std::env::current_exe()?)
                .arg("run").args(&self.child_args)
                .arg("--prompt-file").arg(&task_path)
                .arg("--max-cost").arg(format!("{remaining:.6}"))
                .env("HX_AR_EPISODE", &ep_path)
                .stdin(std::process::Stdio::null())
                .output().await?;
            std::fs::write(ep_dir.join(format!("{tag}.stderr")), &out.stderr)?;
            let report = String::from_utf8_lossy(&out.stdout).lines().rev()
                .find_map(|l| serde_json::from_str::<Value>(l).ok()).unwrap_or(Value::Null);
            std::fs::write(ep_dir.join(format!("{tag}.report.json")), serde_json::to_vec_pretty(&report)?)?;
            // An unreported cost is charged as the whole allowance, so a broken child cannot overspend the run.
            let cost = report["cost_usd"].as_f64().unwrap_or(remaining);
            used += cost;
            self.spent += cost;
            let evals = ledger(&self.dir)?.len() - before;
            let end = if report["finished"] == true { "finished" } else { report["stopped"].as_str().unwrap_or("stopped") };
            eprintln!("hx ar: episode {tag}: {evals} evals, ${cost:.3}, {end}");
            if evals == 0 { break; }
        }
        Ok(())
    }

    /// Held-out score of a snapshot (the search score when there is no select evaluator).
    async fn select(&mut self, snap: &str, search: Option<f64>) -> Result<Option<f64>> {
        let Some(cmd) = self.spec.select.clone() else { return Ok(search) };
        restore(&self.workspace, &self.spec, &self.dir, snap)?;
        let (score, valid, _, _) = evaluate(&self.pol, &self.spec, &cmd).await?;
        Ok(score.filter(|_| valid))
    }

    /// Pick the lineage whose best state scores best on the select evaluator.
    async fn choose(&mut self, round: usize, candidates: &[usize]) -> Result<usize> {
        let rows = ledger(&self.dir)?;
        let mut winner: Option<(usize, f64)> = None;
        let mut scored = vec![];
        for &i in candidates {
            let (snap, search) = self.best(i, &rows);
            let held = if search.is_some() { self.select(&snap, search).await? } else { None };
            scored.push(json!({"lineage": self.lineages[i].id, "snap": snap, "search": search, "select": held}));
            if let Some(h) = held && self.spec.better(h, winner.map(|w| w.1), 0.0) { winner = Some((i, h)); }
        }
        let chosen = winner.map(|w| w.0).unwrap_or(candidates[0]);
        eprintln!("hx ar: round {round} → {}", self.lineages[chosen].id);
        self.selections.push(json!({"round": round, "candidates": scored, "winner": self.lineages[chosen].id}));
        Ok(chosen)
    }
}

fn show(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.6}")).unwrap_or_else(|| "none".into())
}

/// `hx run --ar`: the fork-and-flush schedule over the whole `--max-cost` budget.
pub async fn orchestrate(args: &[String], workspace: &Path, pol: std::sync::Arc<crate::policy::Policy>, child_args: Vec<String>, program: String, budget: f64) -> Result<()> {
    ensure!(crate::trust::is_trusted(workspace), "--ar runs the evaluator commands in .harness/ar.toml; review them and run `hx trust` first");
    let spec = Spec::load(workspace)?;
    ensure!(crate::workflow::git_inventory(workspace)?.is_some(), "--ar requires a Git repository at the workspace root");
    let bad = out_of_scope(workspace, &spec)?;
    ensure!(bad.is_empty(), "--ar starts from committed state: commit or remove changes outside the editable paths: {}", bad.join(", "));
    let _ = args;
    let id = format!("{}-{}", chrono_stamp(), std::process::id());
    let dir = workspace.join(".harness/ar").join(&id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("spec.json"), serde_json::to_vec_pretty(&spec)?)?;
    let mut o = Orchestrator { workspace: workspace.to_path_buf(), dir, spec, program, child_args, budget, spent: 0.0,
        lineages: vec![], selections: vec![], pol };
    let explore = budget * EXPLORE_SHARE;
    let exploit = (budget - explore * (K * ROUNDS) as f64).max(0.0) / ROUNDS as f64;
    eprintln!("hx ar: run {id}: ${budget:.2}, {ROUNDS} rounds × {K} branches at ${explore:.2}, leader segments ${exploit:.2}");
    let mut leader: Option<usize> = None;
    let mut finalists = vec![];
    for round in 0..ROUNDS {
        let mut branches = vec![];
        for k in 0..K {
            let i = o.fork(leader, format!("r{round}b{k}"))?;
            o.episode(i, &format!("r{round}b{k}"), explore).await?;
            branches.push(i);
        }
        let chosen = o.choose(round, &branches).await?;
        finalists.push(chosen);
        o.episode(chosen, &format!("r{round}lead"), exploit).await?;
        leader = Some(chosen);
    }
    // Leaders keep improving after their round's selection, so their final states compete once more on held-out data.
    let rows = ledger(&o.dir)?;
    finalists.dedup();
    let winner = o.choose(ROUNDS, &finalists).await?;
    let (snap, search) = o.best(winner, &rows);
    restore(&o.workspace, &o.spec, &o.dir, &snap)?;
    let report = json!({
        "run": id, "dir": o.dir, "winner": o.lineages[winner].id, "snap": snap, "search": search,
        "select": o.selections.last().and_then(|s| s["candidates"].as_array().and_then(|c| c.iter().find(|c| c["snap"] == snap.as_str())).map(|c| c["select"].clone())),
        "evals": rows.len(), "cost_usd": o.spent, "budget_usd": budget, "selections": o.selections,
    });
    std::fs::write(o.dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}
