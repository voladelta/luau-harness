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
//! Episodes are limited in model turns, not dollars. Rounds refine while refining moves a chosen candidate and explore
//! again when it moves none; a round makes progress when it chooses a new candidate or lifts one by more than
//! `min_delta`, and two rounds in a row without progress end the run. With no valid candidate after round 1 the run
//! stops. The result is the audited good candidates of the last gate, best search score first: several distinct good
//! answers, since the single best score is the one most likely to be an overfit or an evaluator hole.
//!
//! `episode` holds what runs inside an episode (`ar.eval`, `ar.claim`, `ar.claims`, `ar.reset`); `files` holds what
//! crosses its boundary (snapshots, throwaway copies, the sandboxed evaluator, the frozen base).
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

mod episode;
mod files;
pub use episode::{EPISODE, claim, claims, eval, load_episode, reset};
use episode::Episode;
use files::*;

/// Workers side by side in a round, and fewer once the provider has rate-limited an episode of the run.
const WORKERS: usize = 8;
const NARROW: usize = 6;
/// Model turns an episode may take: its whole allowance, in work rather than dollars, so it means the same on any model.
const TURNS: usize = 40;
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

/// Launch settings, frozen at the start and reused on resume.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Launch {
    spec: Spec,
    program: String,
    child_args: Vec<String>,
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
/// finished with a durable record from the killed child's journals, runs what was planned but never started, and
/// continues. Held-out scores, audit verdicts and gates are
/// journaled once and reused.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Planned { batch: String, episodes: Vec<Plan> },
    Started { label: String, plan: String, #[serde(default)] scratch: Option<PathBuf> },
    Finished { label: String, evals: usize, #[serde(default)] interrupted: bool, #[serde(default)] crashed: bool,
        /// Requests the provider refused with HTTP 429, from the child's report.
        #[serde(default)] rate_limits: u64 },
    Selected { snap: String, score: Option<f64> },
    Audited { idea: u32, snap: String, pass: bool, reason: String },
    Gate { round: usize, candidates: Vec<Candidate>, chosen: Vec<u32> },
}

/// What one episode did.
struct Outcome {
    crashed: bool,
    stderr: String,
}

struct Orchestrator {
    workspace: PathBuf,
    dir: PathBuf,
    launch: Launch,
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

    /// Workers per round: `WORKERS`, or `NARROW` once the provider has refused a request of any episode with HTTP 429.
    fn width(&self) -> Result<usize> {
        let limited = self.records()?.iter().any(|r| matches!(r, Record::Finished { rate_limits, .. } if *rate_limits > 0));
        Ok(if limited { NARROW } else { WORKERS })
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

    /// Run a planned batch side by side until every episode has finished, or crashed `MAX_CRASHES` times.
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
            let state = self.state()?;
            let mut runs = vec![];
            for (plan, label) in &pending {
                runs.push(self.episode(plan, label, self.task(plan, label, &state)?));
            }
            let outcomes = futures::future::join_all(runs).await.into_iter().collect::<Result<Vec<_>>>()?;
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
    async fn episode(&self, plan: &Plan, label: &str, task: String) -> Result<Outcome> {
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
        self.journal(&Record::Started { label: label.into(), plan: plan.label.clone(), scratch: Some(scratch.0.clone()) })?;
        eprintln!("hx ar: {label} ({:?}{})", plan.role, if plan.idea > 0 { format!(" #{}", plan.idea) } else { String::new() });
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("run").arg("--workspace").arg(&ws).args(&self.launch.child_args)
            .arg("--prompt-file").arg(&task_path)
            .arg("--max-turns").arg(TURNS.to_string())
            .env("HX_AR_EPISODE", &ep_path)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .spawn()?;
        // Its stdin stays open while this orchestrator lives; if it stops, the child stops with it.
        let (_alive, _tracked) = (child.stdin.take(), child.id().map(crate::prims::Child::track));
        let out = child.wait_with_output().await?;
        std::fs::write(ep_dir.join(format!("{label}.stderr")), &out.stderr)?;
        let report = String::from_utf8_lossy(&out.stdout).lines().rev()
            .find_map(|l| serde_json::from_str::<Value>(l).ok()).unwrap_or(Value::Null);
        std::fs::write(ep_dir.join(format!("{label}.report.json")), serde_json::to_vec_pretty(&report)?)?;
        let logs = keep_logs(&ws, &ep_dir, label)?;
        drop(scratch);
        // Without a report the child's journals say how often it was rate-limited.
        let rate_limits = report["rate_limits"].as_u64().unwrap_or_else(|| rate_limited(&logs));
        let evals = ledger(&self.dir)?.iter().filter(|r| r.episode == label).count();
        let crashed = evals == 0 && report.is_null();
        self.journal(&Record::Finished { label: label.into(), evals, interrupted: false, crashed, rate_limits })?;
        let end = if report["finished"] == true { "finished" } else { report["stopped"].as_str().unwrap_or("stopped") };
        eprintln!("hx ar: {label}: {evals} evals, {end}{}", if rate_limits > 0 { format!(", {rate_limits} rate-limited") } else { String::new() });
        Ok(Outcome { crashed, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
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

    /// The tools an episode has, in the order the prompt guide asks for: grounding before the task, rules last.
    fn tools(&self, role: Role) -> String {
        let spec = self.spec();
        let better = if spec.direction == "max" { "higher" } else { "lower" };
        let mut t = format!("<tools>\n- `ar.eval(\"one-line note\")` scores the editable files ({}) with the owner's frozen evaluator and \
returns {{n, score, valid, se, improved, best, report}}; {better} scores are better. Only states scored by ar.eval count. \
A deterministic evaluator (no `se`) returns a state it already scored from its cache.\n\
- `ar` is a built-in global table like `sh` and `read`; `vars()` lists only your own globals, so call it directly.\n",
            spec.editable.join(", "));
        if matches!(role, Role::Explore | Role::Exploit) {
            t += "- `ar.claims()` lists the claims yours must differ from. `ar.claim(text)` claims your idea or refinement.\n\
- `improved` means better than your best so far (by more than `se` when the evaluator reports one). `ar.reset()` puts \
your best scored state back into the editable paths.\n";
        } else {
            t += "- Your evaluations are probes: recorded, never a candidate. Change the editable files freely to test a question.\n";
        }
        if spec.select.is_some() {
            t += "- Results are judged on held-out data no agent sees: a change with a reason to generalize beats tuning to the reported numbers.\n";
        }
        t + &format!("- You have {TURNS} steps (model turns). At the limit the run stops you, with or without a final answer.\n</tools>\n\n")
    }

    fn task(&self, plan: &Plan, label: &str, state: &State) -> Result<String> {
        let spec = self.spec();
        let (score, report) = self.known(&state.rows, &plan.start);
        let report = clip(&serde_json::to_string_pretty(&report).unwrap_or_default(), REPORT_CHARS);
        let mut context = format!("{}\n\n", self.launch.program.trim_end());
        if plan.role != Role::Research {
            context += &format!("<brief>\n{}\n</brief>\n\n", clip(&state.brief, BRIEF_CHARS));
            if !state.findings.is_empty() { context += &format!("<findings>\n{}</findings>\n\n", state.findings); }
        }
        context += &format!("<starting_state>\nYour workspace holds it (search score {}). Its evaluator report:\n```json\n{report}\n```\n</starting_state>\n\n", show(score));
        let claim = |idea: u32| state.claims.iter().find(|c| c.scope == "explore" && c.n == idea).map_or(String::new(), |c| c.idea.clone());
        let worker_rules = "- ar.eval refuses to score before your claim is accepted: claim first.\n\
- An accepted claim is final: claim only what you will build, never a placeholder, a probe or a test.\n\
- Build and fix; do not optimize: no parameter sweeps, no tuning for score, no switching ideas. Later rounds refine the \
strongest candidates.\n- Every state you score must keep the program's rules and limits (size, time, determinism): check them before each \
ar.eval. An audit discards a state that breaks one.\n";
        let checklist = "Before your final answer, check: [ ] my claim was accepted [ ] my last state was scored by ar.eval and \
is valid [ ] my final answer says what I built, my best score, what worked and what did not.\n";
        Ok(match plan.role {
            Role::Research => format!("{context}{}## Autoresearch research lead ({label})\n\n\
You lead an autoresearch run. Next, {WORKERS} workers will each pursue a different idea to improve the score by changing the \
editable paths ({}). Study the task so their work is grounded, then write the brief every worker gets:\n\
1. The objective, and exactly how the evaluator computes the score (read the code and data you can see).\n\
2. Rules and constraints: what is banned, what makes a result invalid.\n\
3. What the starting state does and where it is weak.\n\
4. Traps: ways a score could rise without real improvement (evaluator quirks, overfitting, degenerate solutions).\n\
5. Open questions worth testing.\n\n<rules>\n- Do not solve the task: the workers do that.\n\
- Keep the study to about 15 steps: an unfinished study leaves every worker without a brief.\n\
- Your final answer is the brief: at most 800 words, facts first, without prescribing one solution.\n</rules>\n",
                self.tools(plan.role), spec.editable.join(", ")),
            Role::Explore => {
                // Only earlier rounds' claims: a retried worker gets its own claim back from ar.claim and must keep building it.
                let round = label.split('-').next();
                let taken: String = state.claims.iter().filter(|c| c.scope == "explore" && c.worker.split('-').next() != round)
                    .map(|c| format!("- #{}: {}\n", c.n, c.idea)).collect();
                let taken = if taken.is_empty() { String::new() } else {
                    format!(" Earlier rounds already claimed these ideas; yours must use a different core approach:\n{}", clip(&taken, BRIEF_CHARS))
                };
                format!("{context}{}## Autoresearch explore worker {label}\n\n\
You are one of {WORKERS} workers exploring side by side; each builds a different core approach to improving the score.\n\n\
1. Generate your idea from your seed: <seed>{}</seed>. List 5 to 7 root-level dimensions along which approaches to this \
task differ fundamentally (not parameters or filters), each with at least 6 options from conventional to radical. Pick one \
option per dimension using a different part of the seed (for example the sum of the character codes of a segment, mod the \
number of options). Commit to the picks even if unusual, and compose one coherent idea: a core approach, not a tweak on \
top of the starting state.{taken}\n\
2. Read `ar.claims()`, then claim your idea within your first few steps: `ar.claim(\"the idea in two or three sentences\")`. \
If it returns `ok = false`, the same core is taken: pick again from a different part of your seed and claim another.\n\
3. Build the simplest version of the idea that runs, and score it with ar.eval within about 10 steps of claiming. Then \
read the report and fix what is broken until the idea works as designed. Your first valid state sets your bar, even \
below the starting state's score.\n\
4. Your final answer is your finding: the idea, what you built, your best score, what worked and what did not, and \
whether the idea is promising.\n\n<rules>\n{worker_rules}</rules>\n\n{checklist}",
                    self.tools(plan.role), plan.seed.as_deref().unwrap_or(label))
            }
            Role::Exploit => {
                let rows: Vec<&Row> = state.rows.iter().filter(|r| r.idea == plan.idea).collect();
                let skip = rows.len().saturating_sub(SHOWN);
                let mut ledger = format!("<ledger>\nn | score | improved | worker | note\n{}", if skip > 0 { format!("({skip} older rows omitted)\n") } else { String::new() });
                for r in &rows[skip..] {
                    let score = match (r.valid, r.score) { (true, Some(s)) => format!("{s:.6}"), _ => "invalid".into() };
                    ledger += &format!("{} | {score} | {} | {} | {}\n", r.n, if r.improved { "yes" } else { "" }, r.episode, r.note.replace('\n', " "));
                }
                format!("{context}{ledger}</ledger>\n\n{}## Autoresearch exploit worker {label}: candidate #{}\n\n\
Candidate #{}'s idea: {}\n\nIt is among the most promising candidates. Several workers refine it side by side, each with \
a different refinement.\n\n\
1. Read the ledger and the findings, then choose one refinement: a change with a reason to generalize.\n\
2. Read `ar.claims()`, then claim it within your first few steps: `ar.claim(\"the refinement in one or two sentences\")`. \
If it returns `ok = false`, the same refinement is taken: choose another.\n\
3. Implement it and score it with ar.eval within about 10 steps of claiming; read the report and fix it if it is broken. \
It improves the candidate when it beats its best ({}).\n\
4. Your final answer is your finding: what you changed, your best score, what worked and what did not.\n\n\
<rules>\n{worker_rules}- One refinement: return once it is built and scored; the next round builds on the best result.\n\
</rules>\n\n{checklist}", self.tools(plan.role), plan.idea, plan.idea, claim(plan.idea), show(plan.best))
            }
            Role::Audit => format!("{context}{}## Autoresearch auditor {label}: candidate #{}\n\n\
Candidate #{} ({}) is about to be kept as a result. Your workspace holds its best state. Decide whether its score is earned:\n\
1. Read the state and explain where its score comes from.\n\
2. Look for an evaluator hole: outcomes the real system would not give, lookahead or leaked information, data the \
solution should not see, hard-coded answers, rule violations, or degenerate behaviour the score rewards by mistake.\n\
3. To test a suspicion, write probe states (for example the trick in isolation) and ar.eval them.\n\n\
<rules>\n- Overfitting alone is not a failure: held-out data handles that.\n\
- Your final answer ends with exactly one line: `VERDICT: PASS` (the score is earned) or `VERDICT: FAIL: reason` (it \
comes from a hole or a rule violation). Without that line the audit counts for nothing.\n</rules>\n",
                self.tools(plan.role), plan.idea, plan.idea, claim(plan.idea)),
        })
    }

    /// Every candidate (an accepted explore claim with a valid improved state, at its best search state), and the ones
    /// that may go on, strongest first. Good: beats the seed on search by more than `min_delta`, and is not worse than the
    /// seed on select (a veto, never a ranking). Ranked good ones first, then by search: workers return their ideas
    /// unpolished, so one below the seed still earns refinement. A failed audit, a copy of the seed, or a state another
    /// candidate ranked higher already holds never goes on.
    async fn candidates(&self) -> Result<(Vec<Candidate>, Vec<u32>)> {
        let seed = self.seed()?;
        let state = self.state()?;
        let records = self.records()?;
        let seed_search = self.known(&state.rows, &seed).0;
        let seed_select = self.select(&seed).await?;
        let mut candidates = vec![];
        let verdict = |idea: u32, at: &str| records.iter().rev().find_map(|r| match r {
            Record::Audited { idea: i, snap, pass, reason } if *i == idea && snap == at => Some((*pass, reason.clone())),
            _ => None,
        });
        for c in state.claims.iter().filter(|c| c.scope == "explore") {
            // Its best state; once an audit has failed one of its states, its best state that passed one, so a
            // refinement that breaks a rule costs only itself (with none that passed, the candidate is out).
            let rows: Vec<&Row> = state.rows.iter().filter(|r| r.idea == c.n && r.improved && r.valid).collect();
            let failed = rows.iter().any(|r| verdict(c.n, &r.snap).is_some_and(|v| !v.0));
            let best = rows.into_iter().filter(|r| !failed || verdict(c.n, &r.snap).is_some_and(|v| v.0))
                .fold(None::<&Row>, |b, r| if self.spec().better(r.score.unwrap(), b.and_then(|b| b.score), 0.0) { Some(r) } else { b });
            let Some(best) = best else { continue };
            let beats = seed_search.is_none_or(|s| self.spec().better(best.score.unwrap(), Some(s), self.spec().min_delta));
            // The veto is computed only where it can decide something, so held-out data is looked at as little as possible.
            let select = if beats { self.select(&best.snap).await? } else { None };
            let held = self.spec().select.is_none()
                || select.is_some_and(|s| seed_select.is_none_or(|b| !self.spec().better(b, Some(s), 0.0)));
            let audit = verdict(c.n, &best.snap);
            candidates.push(Candidate { idea: c.n, claim: c.idea.clone(), snap: best.snap.clone(), search: best.score, select,
                good: beats && held, audit: audit.as_ref().map(|a| a.0), reason: audit.map(|a| a.1).unwrap_or_default() });
        }
        let mut ranked: Vec<&Candidate> = candidates.iter().filter(|c| c.search != seed_search).collect();
        ranked.sort_by(|a, b| b.good.cmp(&a.good).then_with(|| {
            let (a, b) = (a.search.unwrap_or(f64::NAN), b.search.unwrap_or(f64::NAN));
            if self.spec().better(a, Some(b), 0.0) { std::cmp::Ordering::Less } else if self.spec().better(b, Some(a), 0.0) { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal }
        }));
        let mut seen = std::collections::HashSet::new();
        ranked.retain(|c| seen.insert(c.snap.clone()));
        let ranked = ranked.iter().map(|c| c.idea).collect();
        Ok((candidates, ranked))
    }

    /// The gate after `round`: audit the good candidates among the `TOP` strongest that have no verdict yet, in one
    /// batch, then hand the `TOP` strongest to the next round. A candidate whose state fails falls back to its best
    /// state that passed, or drops out; a candidate that takes another's place is audited at a later gate. Only audited
    /// good candidates become results.
    async fn gate(&mut self, round: usize) -> Result<Vec<u32>> {
        let batch = format!("audit{round}");
        // A resumed run finishes the batch it planned, so verdicts come from that batch's own plans.
        let plans = match self.planned(&batch)? {
            Some(plans) => plans,
            None => {
                let (candidates, ranked) = self.candidates().await?;
                let plans: Vec<Plan> = ranked.iter().take(TOP).filter_map(|id| candidates.iter().find(|c| c.idea == *id))
                    .filter(|c| c.good && c.audit.is_none())
                    .map(|c| Plan { label: format!("a{round}-c{}", c.idea), role: Role::Audit, idea: c.idea, start: c.snap.clone(),
                        best: None, seed: None }).collect();
                if !plans.is_empty() { self.plan(&batch, plans.clone())?; }
                plans
            }
        };
        if !plans.is_empty() {
            self.run_batch(&batch).await?;
            self.judge_audits(&plans)?;
        }
        let (candidates, ranked) = self.candidates().await?;
        let chosen: Vec<u32> = ranked.into_iter().take(TOP).collect();
        eprintln!("hx ar: gate {round}: {} candidates, {} good, going on with {:?}", candidates.len(),
            candidates.iter().filter(|c| c.good).count(), chosen);
        self.journal(&Record::Gate { round, candidates, chosen: chosen.clone() })?;
        Ok(chosen)
    }

    /// Journal the verdict of each audit plan that ran and has none yet, from its last attempt's final answer. An
    /// auditor the budget left no room to start, that crashed, or that stopped before a `VERDICT:` line gives none: its
    /// candidate stays unaudited, so it cannot become a result until a later gate audits it.
    fn judge_audits(&self, plans: &[Plan]) -> Result<()> {
        let records = self.records()?;
        let ran = |p: &Plan| records.iter().any(|r| matches!(r, Record::Started { label, plan, .. } if *plan == p.label
            && records.iter().any(|f| matches!(f, Record::Finished { label: l, crashed: false, .. } if l == label))));
        for plan in plans.iter().filter(|p| !audited(&records, p) && ran(p)) {
            let Some((pass, reason)) = verdict(self.answer(&records, &plan.label).as_deref()) else {
                eprintln!("hx ar: audit of #{}: no verdict; it stays unaudited", plan.idea);
                continue;
            };
            eprintln!("hx ar: audit of #{}: {}{}", plan.idea, if pass { "pass" } else { "FAIL" }, if reason.is_empty() { String::new() } else { format!(" ({reason})") });
            self.journal(&Record::Audited { idea: plan.idea, snap: plan.start.clone(), pass, reason })?;
        }
        Ok(())
    }

    async fn run(&mut self, mut records: Vec<Record>) -> Result<Value> {
        // Close episodes that never finished, durably.
        let rows = ledger(&self.dir)?;
        let open: Vec<(String, Option<PathBuf>)> = records.iter().filter_map(|r| match r {
            Record::Started { label, scratch, .. }
                if !records.iter().any(|f| matches!(f, Record::Finished { label: l, .. } if l == label)) =>
                Some((label.clone(), scratch.clone())),
            _ => None,
        }).collect();
        for (label, scratch) in open {
            let evals = rows.iter().filter(|r| r.episode == label).count();
            // The killed child's scratch copy survives a killed orchestrator: keep its journals.
            let rate_limits = match scratch.filter(|s| s.is_dir()) {
                Some(dir) => {
                    let logs = keep_logs(&dir.join("ws"), &self.dir.join("episodes"), &label)?;
                    drop(Scratch(dir));
                    rate_limited(&logs)
                }
                None => 0,
            };
            // Research and audits are judged by their final answer, which a killed child never gave: they are retried
            // like a worker that scored nothing.
            let answer_only = label.starts_with("r0-") || label.starts_with('a');
            let record = Record::Finished { label, evals, interrupted: true, crashed: evals == 0 || answer_only, rate_limits };
            self.journal(&record)?;
            records.push(record);
        }
        if !self.dir.join("seed.json").exists() { self.baseline().await?; }
        let seed = self.seed()?;
        // A select evaluator that cannot score the seed would reject every candidate at the first gate: stop before spending.
        if self.spec().select.is_some() {
            ensure!(self.select(&seed).await?.is_some(), "the select evaluator gives no valid score for the seed; fix it before searching");
        }
        self.plan("research", vec![Plan { label: "r0-research".into(), role: Role::Research, idea: 0, start: seed.clone(), best: None, seed: None }])?;
        self.run_batch("research").await?;
        // Round 1 explores. A round makes progress when it chooses a new candidate or a chosen one beats its score at the
        // last progress by more than `min_delta`, so small steps add up across rounds until they count. After exploring
        // the run refines; it refines again while that moves a chosen candidate at all, and explores again after a
        // refining round that moved none. Two rounds in a row without progress stop the run: every round that makes
        // progress raises a score by more than `min_delta` or adds a candidate, so the run ends.
        let (mut at, mut prev) = (Vec::<(u32, Option<f64>)>::new(), Vec::<(u32, String)>::new());
        let (mut explore, mut quiet) = (true, false);
        for round in 1.. {
            let batch = format!("round{round}");
            let mut gate = self.records()?.into_iter().find_map(|r| match r { Record::Gate { round: g, candidates, chosen } if g == round => Some((candidates, chosen)), _ => None });
            let plans = match self.planned(&batch)? {
                Some(plans) => plans,
                None if gate.is_some() => vec![],
                None => {
                    let workers = self.width()?;
                    let plans = if explore {
                        (1..=workers).map(|w| -> Result<Plan> {
                            Ok(Plan { label: format!("r{round}-w{w}"), role: Role::Explore, idea: 0, start: seed.clone(), best: None,
                                seed: Some(crate::ssot::seed()?) })
                        }).collect::<Result<Vec<_>>>()?
                    } else {
                        let (candidates, chosen) = self.last_gate()?.unwrap_or_default();
                        (0..workers).map(|w| {
                            let c = candidates.iter().find(|c| c.idea == chosen[w % chosen.len()]).expect("chosen candidates are listed");
                            Plan { label: format!("r{round}-w{}", w + 1), role: Role::Exploit, idea: c.idea, start: c.snap.clone(), best: c.search, seed: None }
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
            let chosen: Vec<&Candidate> = chosen.iter().filter_map(|id| candidates.iter().find(|c| c.idea == *id)).collect();
            let progress = chosen.iter().any(|c| match at.iter().find(|(i, _)| *i == c.idea) {
                None => true,
                Some((_, before)) => c.search.is_some_and(|s| self.spec().better(s, *before, self.spec().min_delta)),
            });
            let moved = chosen.iter().any(|c| !prev.contains(&(c.idea, c.snap.clone())));
            if !progress && quiet {
                eprintln!("hx ar: two rounds in a row made no progress; stopping");
                break;
            }
            quiet = !progress;
            explore = !moved && plans.first().is_some_and(|p| p.role == Role::Exploit);
            if progress { at = chosen.iter().map(|c| (c.idea, c.search)).collect(); }
            prev = chosen.iter().map(|c| (c.idea, c.snap.clone())).collect();
        }
        let state = self.state()?;
        ensure!(!state.rows.is_empty(), "autoresearch made no evaluations; see {}", self.dir.join("episodes").display());
        let (candidates, chosen) = self.last_gate()?.unwrap_or_default();
        let portfolio: Vec<&Candidate> = chosen.iter().filter_map(|id| candidates.iter().find(|c| c.idea == *id)).filter(|c| c.good && c.audit == Some(true)).collect();
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
            "evals": state.rows.len(),
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

/// An auditor's verdict from its last `VERDICT:` line: pass, or fail with its reason. None when it gave none.
fn verdict(answer: Option<&str>) -> Option<(bool, String)> {
    let answer = answer?;
    let at = answer.to_ascii_uppercase().rfind("VERDICT:")?;
    let rest = answer[at + "VERDICT:".len()..].trim();
    Some(if rest.to_ascii_uppercase().starts_with("FAIL") {
        (false, rest[4..].trim_start_matches([':', ' ', '-']).lines().next().unwrap_or("").chars().take(300).collect())
    } else {
        (true, String::new())
    })
}

/// At most `max` characters of `text`, marked when cut.
fn clip(text: &str, max: usize) -> String {
    let cut: String = text.chars().take(max).collect();
    if cut.len() < text.len() { cut + "\n... (truncated)" } else { cut }
}

fn show(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.6}")).unwrap_or_else(|| "none".into())
}

/// `hx run --ar`: search until two rounds in a row make no progress, or continue the run in `resume` (a run directory).
pub async fn orchestrate(workspace: &Path, start: Option<(Vec<String>, String)>, resume: Option<PathBuf>) -> Result<()> {
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
        (Some((child_args, program)), None) => {
            ensure!(crate::trust::is_trusted(workspace), "--ar runs the evaluator commands in .harness/ar.toml; review them and run `hx trust` first");
            ensure!(crate::workflow::git_inventory(workspace)?.is_some(), "--ar requires a Git repository at the workspace root");
            let spec = Spec::load(workspace)?;
            let bad = uncommitted(workspace, &spec)?;
            ensure!(bad.is_empty(), "--ar freezes the committed tree: commit or remove changes outside the editable paths: {}", bad.join(", "));
            // `.noindex`: Spotlight skips the run's snapshots, logs and copies.
            let id = format!("{}-{}.noindex", chrono_stamp(), std::process::id());
            let dir = workspace.join(".harness/ar").join(&id);
            std::fs::create_dir_all(&dir)?;
            let commit = freeze(workspace, &dir)?;
            std::fs::write(dir.join("seed"), save_snap(&dir, &capture(workspace, &spec)?)?)?;
            let launch = Launch { spec, program, child_args, commit };
            std::fs::write(dir.join("launch.json"), serde_json::to_vec_pretty(&launch)?)?;
            (dir, launch, vec![])
        }
        (None, None) => bail!("--ar needs a program, or --resume RUN_DIR"),
    };
    let mut o = Orchestrator { workspace: workspace.to_path_buf(), dir, launch };
    eprintln!("hx ar: run {}{}", o.dir.display(), if records.is_empty() { "" } else { ", resumed" });
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
    use super::verdict;


    #[test]
    fn audit_verdicts() {
        assert_eq!(verdict(Some("ok\nVERDICT: PASS")), Some((true, String::new())));
        assert_eq!(verdict(Some("verdict: fail: fills below the market")), Some((false, "fills below the market".into())));
        assert_eq!(verdict(None), None);
        assert_eq!(verdict(Some("ran out")), None);
    }
}
