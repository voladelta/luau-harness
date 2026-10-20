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
/// Flushed restarts within one episode when the agent stops early (a guard; the allowance normally ends it first).
const MAX_ATTEMPTS: usize = 20;

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
async fn evaluate(spec: &Spec, workspace: &Path, cmd: &str) -> Result<(Option<f64>, bool, Option<f64>, Value)> {
    let (stdout, stderr, code, timed_out) = crate::prims::shell(None, cmd, workspace.to_path_buf(), spec.timeout).await
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
    let (score, valid, se, report) = evaluate(spec, &pol.workspace, &spec.eval).await?;
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

/// Journal records: an attempt is `started` before its child runs and `finished` after, so an interrupted attempt is
/// visible on resume and charged its whole allowance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Forked { step: usize, lineage: Lineage },
    Started { step: usize, attempt: usize, allowance: f64 },
    Finished { step: usize, attempt: usize, cost: f64, evals: usize },
    EpisodeDone { step: usize },
    Chosen { step: usize, round: usize, winner: String, candidates: Vec<Value> },
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
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("journal.jsonl"))?;
        writeln!(file, "{}", serde_json::to_string(record)?)?;
        file.sync_all()?;
        Ok(())
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

    /// Best scored state of a lineage: its own valid rows, or the state it started from.
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
        let (_, best) = self.best(id, rows);
        let better = if spec.direction == "max" { "higher" } else { "lower" };
        let mut t = format!("{}\n\n## Autoresearch episode {label}\n\n", self.launch.program.trim_end());
        t += &format!("You are one episode of an autoresearch loop. Improve the score by changing only the editable paths: {}. \
Every other file tracked by Git must stay as committed; put scratch files in tmpdir().\n\n", spec.editable.join(", "));
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
        t
    }

    fn fork(&mut self, step: usize, id: &str, from: &Origin) -> Result<()> {
        let rows = ledger(&self.dir)?;
        let (parent, start, start_score) = match from {
            Origin::Seed => (None, save_snap(&self.dir, &capture(&self.workspace, self.spec())?)?, None),
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

    /// Fresh child runs on lineage `id` until `allowance` is spent or an attempt makes no eval. `used` and `first`
    /// carry what earlier (possibly interrupted) attempts of this step already consumed.
    async fn episode(&mut self, step: usize, id: &str, label: &str, allowance: f64, mut used: f64, first: usize) -> Result<()> {
        let mut crashes = 0;
        for attempt in first..=MAX_ATTEMPTS {
            let remaining = (allowance - used).min(self.launch.budget - self.spent);
            if remaining < allowance * 0.15 || remaining <= 0.0 { break; }
            let rows = ledger(&self.dir)?;
            let (snap, best) = self.best(id, &rows);
            restore(&self.workspace, self.spec(), &self.dir, &snap)?;
            let tag = format!("{label}.{attempt}");
            let episode = Episode { dir: self.dir.clone(), lineage: id.into(), label: tag.clone(), spec: self.spec().clone(), best };
            let ep_dir = self.dir.join("episodes");
            std::fs::create_dir_all(&ep_dir)?;
            let ep_path = ep_dir.join(format!("{tag}.json"));
            std::fs::write(&ep_path, serde_json::to_vec_pretty(&episode)?)?;
            let task_path = ep_dir.join(format!("{tag}.md"));
            std::fs::write(&task_path, self.task(id, &tag, remaining, &rows))?;
            self.journal(&Record::Started { step, attempt, allowance: remaining })?;
            let logs_before = child_logs(&self.workspace);
            eprintln!("hx ar: episode {tag} (lineage {id}, ${remaining:.2}, best {})", show(best));
            let out = tokio::process::Command::new(std::env::current_exe()?)
                .arg("run").args(&self.launch.child_args)
                .arg("--prompt-file").arg(&task_path)
                .arg("--max-cost").arg(format!("{remaining:.6}"))
                .env("HX_AR_EPISODE", &ep_path)
                .stdin(std::process::Stdio::null())
                .output().await?;
            std::fs::write(ep_dir.join(format!("{tag}.stderr")), &out.stderr)?;
            let report = String::from_utf8_lossy(&out.stdout).lines().rev()
                .find_map(|l| serde_json::from_str::<Value>(l).ok()).unwrap_or(Value::Null);
            std::fs::write(ep_dir.join(format!("{tag}.report.json")), serde_json::to_vec_pretty(&report)?)?;
            // A child that died without a report is charged what its journal recorded, unsettled requests at their
            // reservation; without a journal, its whole allowance. Either way a broken child cannot overspend the run.
            let cost = match report["cost_usd"].as_f64() {
                Some(cost) => cost,
                None => child_logs(&self.workspace).difference(&logs_before).map(|log| journal_cost(log)).sum::<Option<f64>>().unwrap_or(remaining),
            };
            let evals = ledger(&self.dir)?.iter().filter(|r| r.episode == tag).count();
            self.journal(&Record::Finished { step, attempt, cost, evals })?;
            used += cost;
            self.spent += cost;
            let end = if report["finished"] == true { "finished" } else { report["stopped"].as_str().unwrap_or("stopped") };
            eprintln!("hx ar: episode {tag}: {evals} evals, ${cost:.3}, {end}");
            if evals == 0 {
                // A crash before any eval is usually the provider (rate limits); retry after a pause, a few times.
                if report.is_null() && crashes < 3 {
                    crashes += 1;
                    let base = std::env::var("HX_AR_RETRY_SECS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(60);
                    let pause = base << crashes;
                    eprintln!("hx ar: episode {tag} crashed before any eval; retrying in {pause}s");
                    tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
                    continue;
                }
                break;
            }
        }
        self.journal(&Record::EpisodeDone { step })
    }

    /// Held-out score of a snapshot (the search score when there is no select evaluator).
    async fn select(&mut self, snap: &str, search: Option<f64>) -> Result<Option<f64>> {
        let Some(cmd) = self.spec().select.clone() else { return Ok(search) };
        restore(&self.workspace, self.spec(), &self.dir, snap)?;
        let (score, valid, _, _) = evaluate(&self.spec().clone(), &self.workspace, &cmd).await?;
        Ok(score.filter(|_| valid))
    }

    /// Pick the lineage whose best state scores best on the select evaluator.
    async fn choose(&mut self, step: usize, round: usize, candidates: &[String]) -> Result<()> {
        let rows = ledger(&self.dir)?;
        let (mut winner, mut scored): (Option<(String, f64)>, Vec<Value>) = (None, vec![]);
        for id in candidates {
            let (snap, search) = self.best(id, &rows);
            let held = if search.is_some() { self.select(&snap, search).await? } else { None };
            scored.push(json!({"lineage": id, "snap": snap, "search": search, "select": held}));
            if let Some(h) = held && self.spec().better(h, winner.as_ref().map(|w| w.1), 0.0) { winner = Some((id.clone(), h)); }
        }
        let winner = winner.map(|w| w.0).unwrap_or_else(|| candidates[0].clone());
        eprintln!("hx ar: round {round} → {winner}");
        let record = Record::Chosen { step, round, winner: winner.clone(), candidates: scored };
        self.journal(&record)?;
        self.apply(&record);
        Ok(())
    }

    fn apply(&mut self, record: &Record) {
        match record {
            Record::Forked { lineage, .. } => self.lineages.push(lineage.clone()),
            Record::Finished { cost, .. } => self.spent += cost,
            Record::Chosen { round, winner, candidates, .. } => {
                self.winners.insert(*round, winner.clone());
                self.chosen.push(json!({"round": round, "winner": winner, "candidates": candidates}));
            }
            Record::Started { .. } | Record::EpisodeDone { .. } => {}
        }
    }

    async fn run(&mut self, records: Vec<Record>) -> Result<Value> {
        let steps = plan(&self.launch.schedule)?;
        let done = |i: usize| records.iter().any(|r| matches!(r, Record::Forked { step, .. } | Record::EpisodeDone { step } | Record::Chosen { step, .. } if *step == i));
        for record in &records { self.apply(record); }
        for (i, step) in steps.iter().enumerate() {
            if done(i) { continue; }
            match step {
                Step::Fork { id, from } => self.fork(i, id, from)?,
                Step::Episode { lineage, round, label, share } => {
                    let id = lineage.clone().unwrap_or_else(|| self.winners[round].clone());
                    // Resume mid-episode: finished attempts count their cost; an interrupted one its whole allowance.
                    let (mut used, mut next) = (0.0, 1);
                    for r in &records {
                        match r {
                            Record::Started { step, attempt, allowance } if *step == i => {
                                if !records.iter().any(|f| matches!(f, Record::Finished { step, attempt: a, .. } if *step == i && a == attempt)) {
                                    used += allowance;
                                    self.spent += allowance;
                                }
                                next = next.max(attempt + 1);
                            }
                            Record::Finished { step, cost, .. } if *step == i => used += cost,
                            _ => {}
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
        let last = self.chosen.last().cloned().context("the schedule ends with a selection")?;
        let winner = last["winner"].as_str().unwrap().to_string();
        let (snap, search) = self.best(&winner, &rows);
        restore(&self.workspace, self.spec(), &self.dir, &snap)?;
        let select = last["candidates"].as_array().and_then(|c| c.iter().find(|c| c["lineage"] == winner.as_str())).map(|c| c["select"].clone());
        let report = json!({
            "run": self.dir.file_name().unwrap().to_string_lossy(), "dir": self.dir, "schedule": self.launch.schedule,
            "winner": winner, "snap": snap, "search": search, "select": select,
            "evals": rows.len(), "cost_usd": self.spent, "budget_usd": self.launch.budget, "selections": self.chosen,
        });
        std::fs::write(self.dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        Ok(report)
    }
}

/// Run journals in the workspace's log directory.
fn child_logs(workspace: &Path) -> std::collections::BTreeSet<PathBuf> {
    std::fs::read_dir(workspace.join(".harness/log")).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect()
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
    ensure!(crate::trust::is_trusted(workspace), "--ar runs the evaluator commands in .harness/ar.toml; review them and run `hx trust` first");
    ensure!(crate::workflow::git_inventory(workspace)?.is_some(), "--ar requires a Git repository at the workspace root");
    let (dir, launch, records) = match (start, resume) {
        (_, Some(dir)) => {
            let dir = dir.canonicalize().with_context(|| format!("autoresearch run {}", dir.display()))?;
            ensure!(dir.parent() == Some(&workspace.join(".harness/ar").canonicalize()?), "--resume for --ar takes a run directory under .harness/ar");
            let launch: Launch = serde_json::from_slice(&std::fs::read(dir.join("launch.json"))?)?;
            ensure!(!dir.join("report.json").exists(), "autoresearch run {} already finished", dir.display());
            let text = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap_or_default();
            let records = text.lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str).collect::<Result<Vec<Record>, _>>()?;
            (dir, launch, records)
        }
        (Some((child_args, program, budget)), None) => {
            let spec = Spec::load(workspace)?;
            let schedule = std::env::var("HX_AR_SCHEDULE").unwrap_or_else(|_| "forkflush".into());
            plan(&schedule)?;
            let bad = out_of_scope(workspace, &spec)?;
            ensure!(bad.is_empty(), "--ar starts from committed state: commit or remove changes outside the editable paths: {}", bad.join(", "));
            let id = format!("{}-{}", chrono_stamp(), std::process::id());
            let dir = workspace.join(".harness/ar").join(&id);
            std::fs::create_dir_all(&dir)?;
            let launch = Launch { spec, program, child_args, budget, schedule };
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
