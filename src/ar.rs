//! Autoresearch (`hx run --ar`): a manager loop that improves a score by changing a task's editable files, and returns
//! a portfolio of distinct good candidates rather than one best state (the single best score is the state most likely
//! to be an overfit or an evaluator hole).
//!
//! The task supplies `.harness/ar.toml` (`Spec`) and the program text (`--prompt-file`). At launch the committed tree
//! is frozen into the run's `base`; every episode is a fresh child `hx run` in a throwaway copy of it, and only
//! snapshots of the editable paths ever leave an episode (`files`). The run directory lives in the state directory,
//! sealed from file tools and hidden from confined shell commands; on Linux, Landlock confines episodes and evaluators
//! to their copies (network isolation is external), and a spec with held-out or grader data refuses to run without it
//! unless the owner says so.
//!
//! The orchestrator is the only writer of run state: one hash-chained journal (`Record`s in an `EventLog`) and the
//! content-addressed blob store beside it, which holds snapshots, reports and the episodes' answers. Children never
//! touch the run directory; they ask the orchestrator over a channel (`episode`), which scores and journals for them.
//!
//! A run is research, then rounds of workers side by side, each followed by a gate. Before a round the orchestrator
//! plans one idea per worker (`ideas`): String Seed of Thought samples, one dedupe call, duplicates sampled again. Round 1
//! explores: each planned idea is a candidate. The gate is a pure function of the journal (`gate`): for each candidate
//! it derives its frontier, the best state no check rejected, which the next round refines, and its result, the best
//! state that beats the seed by the margin, passes the held-out `select` veto and passed an audit. Exploit rounds refine
//! the strongest frontiers; one that moves none explores again. Two rounds without progress, or `max_rounds`, end the
//! run, and the portfolio is the strongest results. The held-out score is only ever a veto: ranking on it as well would
//! select on it every round until it is as optimistic as the search score.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::eventlog::{EventLog, read_blob};

mod episode;
pub(crate) mod files;
mod ssot;
pub use episode::{Episode, eval, load_episode, reset, submit_verdict};
use files::*;

/// Model turns an episode may take: its whole allowance, in work rather than dollars, so it means the same on any model.
const TURNS: usize = 40;
/// Candidates a gate hands to the next round, and the size of the final portfolio.
const TOP: usize = 3;
/// Evaluations shown to a worker, and findings (the most recent ones).
const SHOWN: usize = 40;
const FINDINGS: usize = 16;
/// Characters of an evaluator report, of a brief and of one finding shown to an episode.
const REPORT_CHARS: usize = 12_000;
const BRIEF_CHARS: usize = 8_000;
const FINDING_CHARS: usize = 1_500;
/// Crashes of a planned episode (a child that ends with neither a report nor an eval, usually provider errors) before
/// it is given up.
const MAX_CRASHES: usize = 3;
/// Times a duplicate idea is sampled again before it is accepted as it is.
const RESAMPLES: usize = 2;
/// The smallest difference that counts as better, relative to the score's magnitude (at least 1): rounding never does.
const EPSILON: f64 = 1e-9;
/// Seconds a child may run past `episode_secs` (finishing its answer, stopping its commands) before the watchdog kills
/// it: one that ignores its own deadline would otherwise hold its round forever.
const GRACE_SECS: f64 = 120.0;
/// The run's format, in its launch record. A run started by an hx with another format is refused on resume.
const FORMAT: u32 = 3;

/// Role prompts, kept in `stdlib/std/ar` beside the RPI phase documents and embedded the same way.
const TOOLS: &str = include_str!("../stdlib/std/ar/tools.md");
const RESEARCH: &str = include_str!("../stdlib/std/ar/research.md");
const EXPLORE: &str = include_str!("../stdlib/std/ar/explore.md");
const EXPLOIT: &str = include_str!("../stdlib/std/ar/exploit.md");
const AUDIT: &str = include_str!("../stdlib/std/ar/audit.md");
const IDEA: &str = include_str!("../stdlib/std/ar/idea.md");
const DEDUPE: &str = include_str!("../stdlib/std/ar/dedupe.md");

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction { #[default] Max, Min }

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
    #[serde(default)]
    pub direction: Direction,
    #[serde(default = "Spec::timeout")]
    pub timeout: f64,
    /// The smallest lift that counts as better (`margin`), together with the evaluator's reported `se`.
    #[serde(default)]
    pub min_delta: f64,
    /// Wall-clock seconds an episode may take (`--max-wall` for the child). A round waits for its slowest worker, so
    /// without it one worker's long benchmarks hold every other candidate back.
    #[serde(default = "Spec::episode_secs")]
    pub episode_secs: f64,
    /// Workers side by side in a round.
    #[serde(default = "Spec::workers")]
    pub workers: usize,
    /// Rounds a run takes at most. The progress rule ends a run whose scores are bounded; scores need not be, so this
    /// ends every run.
    #[serde(default = "Spec::max_rounds")]
    pub max_rounds: usize,
    /// Evaluators at once across the run (default: half the CPUs, at least one), so evaluations side by side do not
    /// skew each other's timings or time out by load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_jobs: Option<usize>,
}
impl Spec {
    fn timeout() -> f64 { 600.0 }
    fn episode_secs() -> f64 { 900.0 }
    fn workers() -> usize { 8 }
    fn max_rounds() -> usize { 20 }
    pub fn load(workspace: &Path) -> Result<Spec> {
        let path = workspace.join(".harness/ar.toml");
        let text = std::fs::read_to_string(&path).with_context(|| format!("--ar needs {}", path.display()))?;
        let spec: Spec = toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        spec.check(workspace)?;
        Ok(spec)
    }
    /// The rules a spec must keep, at launch and again on resume.
    fn check(&self, workspace: &Path) -> Result<()> {
        let spec = self;
        ensure!(!spec.editable.is_empty(), "ar.toml: editable must name at least one path");
        ensure!(spec.episode_secs.is_finite() && spec.episode_secs >= 60.0, "ar.toml: episode_secs must be at least 60");
        ensure!((1..=32).contains(&spec.workers), "ar.toml: workers must be between 1 and 32");
        ensure!(spec.max_rounds >= 1, "ar.toml: max_rounds must be at least 1");
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
        ensure!(spec.timeout > 0.0 && spec.min_delta >= 0.0, "ar.toml: timeout must be positive and min_delta nonnegative");
        Ok(())
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
    /// Is `a` strictly ahead of `b` in the spec's direction? For ranking; decisions use `better`.
    fn ahead(&self, a: f64, b: f64) -> bool {
        match self.direction { Direction::Max => a > b, Direction::Min => a < b }
    }
    /// Ranking order: the better score first.
    fn order(&self, a: f64, b: f64) -> std::cmp::Ordering {
        match self.direction { Direction::Max => b.total_cmp(&a), Direction::Min => a.total_cmp(&b) }
    }
    /// Is `a` better than `b` by more than `margin` (and more than `EPSILON` of `b`'s magnitude)?
    fn better(&self, a: f64, b: Option<f64>, margin: f64) -> bool {
        let Some(b) = b else { return true };
        let margin = margin.max(EPSILON * b.abs().max(1.0));
        match self.direction { Direction::Max => a > b + margin, Direction::Min => a < b - margin }
    }
}

/// The one margin by which a score must beat another to count as better: a worker's `improved`, a candidate's
/// `good`, a round's progress and an audit of a lifted result all use it. `min_delta`, or the standard error of the
/// difference of the two scores when the evaluator reports `se` and that is larger: a lift within the evaluator's noise
/// is luck, not progress.
pub(crate) fn margin(spec: &Spec, se_a: Option<f64>, se_b: Option<f64>) -> f64 {
    let (a, b) = (se_a.unwrap_or(0.0), se_b.unwrap_or(0.0));
    spec.min_delta.max((a * a + b * b).sqrt())
}


/// What an episode is for. Research writes the brief; explore workers build a new idea; exploit workers build a
/// refinement of one candidate; auditors judge whether a candidate's score is real. Research and audit evaluations are
/// probes: recorded, never a candidate's state.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role { Research, Explore, Exploit, Audit }

/// Paths a child's policy must seal, so that even with --yes its agent cannot read or change them: every autoresearch
/// run of the owner's workspace and the evaluators' roots, which the orchestrator resolved before it started the
/// episode (one it cannot resolve stops the run, so nothing is left unsealed), and the child's own journal.
pub fn sealed(e: &Episode) -> Vec<PathBuf> {
    e.sealed.iter().cloned().chain([e.log.clone()]).collect()
}

/// The evaluator a score comes from: the agent-visible search evaluator, or the held-out select evaluator.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
enum Tier { Search, Select }

/// Launch settings, frozen in the journal's first record and reused on resume. The seed is the snapshot of the
/// workspace's editable paths at launch.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Launch {
    format: u32,
    spec: Spec,
    program: String,
    child_args: Vec<String>,
    commit: String,
    seed: String,
}

/// One planned episode. A batch (the research, a round's workers, a gate's audits) is planned whole, ideas included,
/// before any of it starts, so a resumed run finishes exactly the batch it was in.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Plan {
    label: String,
    role: Role,
    /// The round it belongs to: 0 for the research, the gate's round for audits.
    round: usize,
    /// The candidate an explore worker founds, an exploit worker refines or an auditor judges (0 for the research).
    candidate: u32,
    start: String,
    best: Option<f64>,
    /// An explore worker's idea, or an exploit worker's refinement, as the orchestrator planned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idea: Option<String>,
}

/// How an episode attempt ended. Only a crash counts toward `MAX_CRASHES`: an attempt cut short by a stopped
/// orchestrator says nothing about the episode.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    /// Journaled before its child starts; an attempt with nothing after it was left open by a stopped orchestrator.
    Started,
    /// The child ended by itself, with a report or with evals.
    Done,
    /// The child ended with neither, usually after provider errors: retried.
    Crashed,
    /// The orchestrator stopped while the child ran; recorded on resume.
    Interrupted,
    /// The watchdog killed a child that outlived its wall-clock cap: done, with whatever it scored.
    TimedOut,
}

/// Journal records: everything a run decides from. A batch is `planned` whole before it starts; an `episode` attempt is
/// journaled as started before its child runs and again when it ends (on resume, for one a stopped orchestrator left
/// open), with its report's blob. A `score` is journaled once per evaluator and snapshot, and reused; an `eval` makes a
/// state a member of a candidate (or a probe); a `verdict` is an auditor's, once per snapshot; a `gate` records only
/// which candidates go on (`last`: the run's final gate). Frontiers, results and progress are derived, never stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Launch(Launch),
    Planned { batch: String, episodes: Vec<Plan> },
    Episode { plan: String, attempt: usize, role: Role, outcome: Outcome, #[serde(default, skip_serializing_if = "Option::is_none")] report_ref: Option<String> },
    Score { tier: Tier, snap: String, score: Option<f64>, valid: bool, se: Option<f64>, #[serde(default, skip_serializing_if = "Option::is_none")] report_ref: Option<String> },
    Eval { n: usize, episode: String, candidate: u32, snap: String, note: String },
    Verdict { snap: String, pass: bool, reason: String },
    Gate { round: usize, last: bool, chosen: Vec<u32> },
}
/// The journal's own kinds; the rest are the orchestrator's model calls (`llm`, `model_intent`, ...).
const KINDS: &[&str] = &["launch", "planned", "episode", "score", "eval", "verdict", "gate"];

/// The records of a run's journal (verified: a torn final line is dropped, damage anywhere else is an error).
fn load(dir: &Path) -> Result<Vec<Record>> {
    crate::eventlog::load(&dir.join("journal.jsonl"))?.into_iter()
        .filter(|v| v["kind"].as_str().is_some_and(|k| KINDS.contains(&k)))
        .map(|v| serde_json::from_value(v).context("an autoresearch journal record")).collect()
}

/// An attempt's label: the plan's (`r1-w3`), then `r1-w3.2` and on for retries.
fn attempt_label(plan: &str, attempt: usize) -> String {
    if attempt <= 1 { plan.to_string() } else { format!("{plan}.{attempt}") }
}

/// A score as journaled.
#[derive(Clone, Debug, Default)]
struct Score {
    score: Option<f64>,
    valid: bool,
    se: Option<f64>,
    report: Option<String>,
}
impl Score {
    fn value(&self) -> Option<f64> { self.score.filter(|_| self.valid) }
}

/// What the records say, derived on demand: the seed, each snapshot's score per tier, verdicts, evaluations in order,
/// every plan and the candidates (explore plans, with their ideas).
#[derive(Default)]
struct Book {
    seed: String,
    scores: HashMap<(Tier, String), Score>,
    verdicts: HashMap<String, (bool, String)>,
    /// (n, attempt label, candidate, snap, note)
    evals: Vec<(usize, String, u32, String, String)>,
    plans: HashMap<String, Plan>,
    candidates: Vec<(u32, String)>,
}
impl Book {
    fn of(records: &[Record]) -> Book {
        let mut b = Book::default();
        for r in records {
            match r {
                Record::Launch(l) => b.seed = l.seed.clone(),
                Record::Score { tier, snap, score, valid, se, report_ref } => {
                    b.scores.entry((*tier, snap.clone())).or_insert(Score { score: *score, valid: *valid, se: *se, report: report_ref.clone() });
                }
                Record::Verdict { snap, pass, reason } => { b.verdicts.entry(snap.clone()).or_insert((*pass, reason.clone())); }
                Record::Eval { n, episode, candidate, snap, note } => b.evals.push((*n, episode.clone(), *candidate, snap.clone(), note.clone())),
                Record::Planned { episodes, .. } => for p in episodes {
                    if p.role == Role::Explore { b.candidates.push((p.candidate, p.idea.clone().unwrap_or_default())); }
                    b.plans.insert(p.label.clone(), p.clone());
                },
                _ => {}
            }
        }
        b
    }
    fn score(&self, tier: Tier, snap: &str) -> Option<&Score> { self.scores.get(&(tier, snap.to_string())) }
    fn search(&self, snap: &str) -> Option<f64> { self.score(Tier::Search, snap).and_then(Score::value) }
    /// The held-out score of a snapshot: None when not computed, Some(None) when the select evaluator found it invalid.
    fn select(&self, snap: &str) -> Option<Option<f64>> { self.score(Tier::Select, snap).map(Score::value) }
    fn idea(&self, candidate: u32) -> String {
        self.candidates.iter().find(|c| c.0 == candidate).map(|c| c.1.clone()).unwrap_or_default()
    }
    /// Is a held-out score a veto: invalid there, or worse than the seed's?
    fn vetoes(&self, spec: &Spec, score: Option<f64>) -> bool {
        score.is_none_or(|x| self.select(&self.seed).flatten().is_some_and(|b| spec.ahead(b, x)))
    }
}

/// One state of a candidate as a gate sees it: its search score, and its held-out score once computed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Scored {
    snap: String,
    search: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se: Option<f64>,
    #[serde(default)]
    select: Option<f64>,
}

/// A gate's decision for one candidate (an explore worker's planned idea with a valid state other than the seed).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Decision {
    candidate: u32,
    idea: String,
    /// The best valid search state no check rejected: what the next round refines.
    frontier: Scored,
    /// The frontier beats the seed by the margin and passed the held-out veto.
    good: bool,
    /// The frontier's audit verdict, when it has one.
    audit: Option<bool>,
    /// The best state that beats the seed by the margin, passed the held-out veto and passed an audit: what the
    /// candidate contributes to the portfolio.
    result: Option<Scored>,
}

/// What a gate decides: each candidate's decision, the candidates ranked for the next round, and the frontiers whose
/// held-out score it needs before its decisions are final.
#[derive(Debug, Default)]
struct Gated {
    decisions: Vec<Decision>,
    ranked: Vec<u32>,
    need_select: Vec<String>,
}

/// The gate, a pure function of the journal. Per candidate, each state counts once and only the seed's own bytes are
/// excluded: a state that merely ties the seed's score is a candidate's state like any other, not good, ranked by search.
///
/// A state is rejected by a failed audit or by the held-out veto (worse than the seed on `select`, or invalid there).
/// The frontier is the best state no check rejected; it is good when it beats the seed by `margin` and passed the veto.
/// The held-out score is looked at only where it can decide something: a frontier that beats the seed and has none yet
/// is listed in `need_select`, and the caller gates again once it is journaled (a veto moves the frontier back, exactly
/// like a failed audit). The result is the best state that beats the seed, passed the veto and passed an audit.
///
/// Candidates rank good ones first, then by search (workers return their ideas unpolished, so one below the seed still
/// earns refinement), then by id; a frontier another candidate ranked higher already holds is left out.
fn gate(spec: &Spec, records: &[Record]) -> Gated {
    let book = Book::of(records);
    let seed = book.score(Tier::Search, &book.seed).cloned().unwrap_or_default();
    let beats = |s: &Scored| spec.better(s.search, seed.value(), margin(spec, s.se, seed.se));
    // Some(true): vetoed; None: not computed.
    let vetoed = |s: &Scored| -> Option<bool> {
        if spec.select.is_none() { return Some(false); }
        Some(book.vetoes(spec, book.select(&s.snap)?))
    };
    let verdict = |s: &Scored| book.verdicts.get(&s.snap).map(|v| v.0);
    let best = |states: &mut dyn Iterator<Item = &Scored>| -> Option<Scored> {
        states.fold(None::<&Scored>, |b, s| if b.is_none_or(|b| spec.ahead(s.search, b.search)) { Some(s) } else { b })
            .map(|s| Scored { select: book.select(&s.snap).flatten(), ..s.clone() })
    };
    let mut out = Gated::default();
    for (id, idea) in &book.candidates {
        let mut states: Vec<Scored> = vec![];
        for (_, _, _, snap, _) in book.evals.iter().filter(|e| e.2 == *id && e.3 != book.seed) {
            let (Some(search), Some(score)) = (book.search(snap), book.score(Tier::Search, snap)) else { continue };
            if !states.iter().any(|s| s.snap == *snap) { states.push(Scored { snap: snap.clone(), search, se: score.se, select: None }); }
        }
        let rejected = |s: &Scored| verdict(s) == Some(false) || vetoed(s) == Some(true);
        let Some(frontier) = best(&mut states.iter().filter(|s| !rejected(s))) else { continue };
        if beats(&frontier) && vetoed(&frontier).is_none() { out.need_select.push(frontier.snap.clone()); }
        let result = best(&mut states.iter().filter(|s| beats(s) && vetoed(s) == Some(false) && verdict(s) == Some(true)));
        out.decisions.push(Decision { candidate: *id, idea: idea.clone(), good: beats(&frontier) && vetoed(&frontier) == Some(false),
            audit: verdict(&frontier), frontier, result });
    }
    let mut order: Vec<&Decision> = out.decisions.iter().collect();
    order.sort_by(|a, b| b.good.cmp(&a.good).then_with(|| spec.order(a.frontier.search, b.frontier.search)).then(a.candidate.cmp(&b.candidate)));
    let mut seen = HashSet::new();
    out.ranked = order.into_iter().filter(|d| seen.insert(d.frontier.snap.clone())).map(|d| d.candidate).collect();
    out
}

/// The frontiers a gate audits, among the `TOP` it hands on: good ones without a verdict whose candidate has no result
/// yet (it has just become good, or its earlier state failed). A candidate that already has a result is audited again
/// only at the last gate, and only when its frontier beats that result by the margin: auditing every lift of every
/// candidate every round costs an episode per candidate per round for a result that only the last gate fixes.
fn audits<'a>(spec: &Spec, gated: &'a Gated, last: bool) -> Vec<&'a Decision> {
    gated.ranked.iter().take(TOP).filter_map(|id| gated.decisions.iter().find(|d| d.candidate == *id))
        .filter(|d| d.good && d.audit.is_none())
        .filter(|d| d.result.as_ref().is_none_or(|r| last && spec.better(d.frontier.search, Some(r.search), margin(spec, d.frontier.se, r.se))))
        .collect()
}

/// The portfolio: the distinct results of a gate's decisions, best search score first, at most `TOP`.
fn portfolio(spec: &Spec, decisions: &[Decision]) -> Vec<Value> {
    let mut results: Vec<(&Decision, &Scored)> = decisions.iter().filter_map(|d| d.result.as_ref().map(|r| (d, r))).collect();
    results.sort_by(|a, b| spec.order(a.1.search, b.1.search).then(a.0.candidate.cmp(&b.0.candidate)));
    let mut seen = HashSet::new();
    results.into_iter().filter(|(_, r)| seen.insert(r.snap.clone())).take(TOP)
        .map(|(d, r)| json!({"candidate": d.candidate, "idea": d.idea, "snap": r.snap, "search": r.search, "se": r.se, "select": r.select}))
        .collect()
}

/// The attempts of a planned episode, in order: (attempt, outcome once it ended, label).
fn attempts(records: &[Record], plan: &str) -> Vec<(usize, Option<Outcome>, String)> {
    let mut out: Vec<(usize, Option<Outcome>, String)> = vec![];
    for r in records {
        let Record::Episode { plan: p, attempt, outcome, .. } = r else { continue };
        if p != plan { continue; }
        match out.iter_mut().find(|a| a.0 == *attempt) {
            Some(a) => a.1 = Some(*outcome),
            None => out.push((*attempt, (*outcome != Outcome::Started).then_some(*outcome), attempt_label(plan, *attempt))),
        }
    }
    out
}

/// The plans of a batch still to run, each with its next attempt and its crashes so far. A plan is settled by an
/// attempt that finished or timed out, or an interrupted one that left what its role is for (a worker's evals, an
/// auditor's verdict); it is given up after `MAX_CRASHES` crashes. Interrupted attempts are no crashes, so stopping the
/// orchestrator any number of times never uses up an episode, and a batch whose attempts were all interrupted is never
/// taken for finished.
fn pending(plans: Vec<Plan>, records: &[Record]) -> Vec<(Plan, usize, usize)> {
    let book = Book::of(records);
    plans.into_iter().filter_map(|plan| {
        let tries = attempts(records, &plan.label);
        let settled = tries.iter().any(|(_, outcome, label)| match outcome {
            Some(Outcome::Done | Outcome::TimedOut) => true,
            Some(Outcome::Interrupted) => match plan.role {
                Role::Explore | Role::Exploit => book.evals.iter().any(|e| e.1 == *label),
                Role::Audit => book.verdicts.contains_key(&plan.start),
                Role::Research => false,
            },
            _ => false,
        });
        let crashes = tries.iter().filter(|t| t.1 == Some(Outcome::Crashed)).count();
        (!settled && crashes < MAX_CRASHES).then_some((plan, tries.len() + 1, crashes))
    }).collect()
}

/// A worker's best score and the state that holds it, from its attempt's evaluations in order: a state counts when it
/// beats the best so far by the evaluator's noise (the bar starts at the frontier an exploit worker refines); the state
/// it started from never does.
fn progress(spec: &Spec, book: &Book, plan: &Plan, label: &str) -> (Option<f64>, Option<String>) {
    let (mut best, mut at) = (plan.best, None);
    for (_, _, _, snap, _) in book.evals.iter().filter(|e| e.1 == label && e.3 != plan.start) {
        let Some(s) = book.score(Tier::Search, snap) else { continue };
        if let Some(v) = s.value().filter(|v| spec.better(*v, best, margin(spec, s.se, None))) { (best, at) = (Some(v), Some(snap.clone())); }
    }
    (best, at)
}

/// What a child that ran reported back to its batch.
struct Ran {
    crashed: bool,
    stderr: String,
}

struct Orchestrator {
    workspace: PathBuf,
    dir: PathBuf,
    launch: Launch,
    /// The hx binary episodes run (`hx_exe`).
    exe: PathBuf,
    /// The run's journal and blob store, and its records as journaled so far (only this process writes them).
    log: Arc<EventLog>,
    records: std::sync::Mutex<Vec<Record>>,
    /// Evaluator slots (`eval_jobs`), and one evaluation at a time per tier and snapshot, so each is scored once.
    slots: tokio::sync::Semaphore,
    scoring: std::sync::Mutex<HashMap<(Tier, String), Arc<tokio::sync::Mutex<()>>>>,
    /// The host the orchestrator's own model calls (idea planning) go through, made at the first one.
    planner: std::sync::Mutex<Option<Arc<crate::host::Host>>>,
}

impl Orchestrator {
    fn new(workspace: &Path, dir: PathBuf, launch: Launch, exe: PathBuf, log: Arc<EventLog>, records: Vec<Record>) -> Orchestrator {
        Orchestrator { workspace: workspace.to_path_buf(), slots: tokio::sync::Semaphore::new(eval_jobs(&launch.spec)), dir, launch, exe, log,
            records: records.into(), scoring: Default::default(), planner: Default::default() }
    }

    fn spec(&self) -> &Spec { &self.launch.spec }

    fn records(&self) -> Vec<Record> { self.records.lock().unwrap().clone() }

    fn book(&self) -> Book { Book::of(&self.records.lock().unwrap()) }

    /// Journal what `make` returns given the records so far, durably, atomically with that look: numbering an eval, or
    /// recording a batch, a score or a verdict only once.
    fn journal_if(&self, make: impl FnOnce(&[Record]) -> Option<Record>) -> Result<Option<Record>> {
        let mut records = self.records.lock().unwrap();
        let Some(record) = make(&records) else { return Ok(None) };
        let Value::Object(mut fields) = serde_json::to_value(&record)? else { bail!("journal records are objects") };
        let kind = fields.remove("kind").and_then(|k| k.as_str().map(String::from)).context("record kind")?;
        self.log.write(&kind, Value::Object(fields), true)?;
        records.push(record.clone());
        Ok(Some(record))
    }

    fn journal(&self, record: Record) -> Result<()> { self.journal_if(|_| Some(record)).map(drop) }

    fn blob(&self, value: &Value) -> Result<Option<String>> {
        if value.is_null() { return Ok(None); }
        Ok(Some(self.log.blob(&serde_json::to_vec(value)?)?))
    }

    fn read(&self, blob: Option<&str>) -> Value {
        blob.and_then(|b| read_blob(&self.dir, b).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
    }

    /// The score of `snap` on a tier: evaluated once and journaled, then reused. At most `eval_jobs` evaluators run at
    /// once. An evaluation cut short by this process's shutdown records nothing. A select report is never kept: it can
    /// hold held-out data.
    async fn score(&self, tier: Tier, snap: &str) -> Result<Score> {
        let known = || self.book().score(tier, snap).cloned();
        if let Some(s) = known() { return Ok(s); }
        let one = self.scoring.lock().unwrap().entry((tier, snap.to_string())).or_default().clone();
        let _one = one.lock().await;
        if let Some(s) = known() { return Ok(s); }
        let cmd = match tier { Tier::Search => self.spec().eval.clone(), Tier::Select => self.spec().select.clone().context("no select evaluator")? };
        let files = load_snap(&self.dir, snap)?;
        let (score, valid, se, report) = {
            let _slot = self.slots.acquire().await?;
            evaluate(self.spec(), &self.dir, snap, &files, &cmd, tier == Tier::Select).await?
        };
        ensure!(!crate::prims::stopping(), "stopped while evaluating; nothing was recorded");
        let report_ref = if tier == Tier::Search { self.blob(&report)? } else { None };
        self.journal(Record::Score { tier, snap: snap.into(), score, valid, se, report_ref: report_ref.clone() })?;
        Ok(Score { score, valid, se, report: report_ref })
    }

    /// The gate on everything journaled so far. The held-out scores it needs are computed side by side and journaled,
    /// and it gates again, until it needs none (each pass computes at most one per candidate, and a veto only ever moves
    /// a frontier to a state not yet looked at).
    async fn decide(&self) -> Result<Gated> {
        loop {
            let gated = gate(self.spec(), &self.records());
            if gated.need_select.is_empty() { return Ok(gated); }
            futures::future::join_all(gated.need_select.iter().map(|snap| self.score(Tier::Select, snap))).await.into_iter().collect::<Result<Vec<_>>>()?;
        }
    }

    /// The gate journaled for `round` (or the last one), as it decided: the gate function over the records before it.
    fn gated(&self, round: Option<usize>) -> Option<(Vec<Decision>, Vec<u32>)> {
        let records = self.records();
        let at = records.iter().rposition(|r| matches!(r, Record::Gate { round: g, last, .. } if round.is_none_or(|n| *g == n && !last)))?;
        let Record::Gate { chosen, .. } = &records[at] else { return None };
        Some((gate(self.spec(), &records[..at]).decisions, chosen.clone()))
    }

    /// The final answer of the last attempt of a planned episode that reported one: a worker's finding, the brief.
    fn answer(&self, records: &[Record], plan: &str) -> Option<String> {
        let blob = records.iter().rev().find_map(|r| match r {
            Record::Episode { plan: p, report_ref: Some(blob), .. } if p == plan => Some(blob.as_str()),
            _ => None,
        })?;
        self.read(Some(blob))["final_answer"].as_str().map(str::trim).filter(|a| !a.is_empty()).map(String::from)
    }

    /// The episodes of a journaled batch.
    fn planned(&self, batch: &str) -> Option<Vec<Plan>> {
        self.records().into_iter().find_map(|r| match r { Record::Planned { batch: b, episodes } if b == batch => Some(episodes), _ => None })
    }

    /// Journal a batch once.
    fn plan(&self, batch: &str, episodes: Vec<Plan>) -> Result<()> {
        self.journal_if(|records| (!records.iter().any(|r| matches!(r, Record::Planned { batch: b, .. } if b == batch)))
            .then(|| Record::Planned { batch: batch.into(), episodes })).map(drop)
    }

    /// Run a planned batch side by side until every episode is settled or has crashed `MAX_CRASHES` times.
    async fn run_batch(&self, batch: &str) -> Result<()> {
        let mut crashes = 0;
        loop {
            let pending = pending(self.planned(batch).unwrap_or_default(), &self.records());
            if pending.is_empty() { return Ok(()); }
            let retries_left = pending.iter().any(|(.., crashed)| crashed + 1 < MAX_CRASHES);
            let view = self.view();
            let mut runs = vec![];
            for (plan, attempt, _) in &pending {
                runs.push(self.episode(plan, *attempt, self.task(plan, &attempt_label(&plan.label, *attempt), &view)?));
            }
            let ran = futures::future::join_all(runs).await.into_iter().collect::<Result<Vec<_>>>()?;
            if !ran.iter().all(|o| o.crashed) { crashes = 0; continue; }
            crashes += 1;
            // Before the first eval of the whole run, repeated crashes mean misconfiguration: stop, don't wait hours.
            if self.book().evals.is_empty() && crashes >= MAX_CRASHES {
                let last: Vec<&str> = ran[0].stderr.trim().lines().rev().take(15).collect();
                bail!("episode children keep failing before any eval; last error:\n{}", last.into_iter().rev().collect::<Vec<_>>().join("\n"));
            }
            if !retries_left { return Ok(()); }
            let base = std::env::var("HX_AR_RETRY_SECS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(60);
            let pause = base << crashes;
            eprintln!("hx ar: every episode of {batch} crashed before any eval; retrying in {pause}s");
            tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
        }
    }

    /// The watchdog's limit for a child: its wall-clock cap and `GRACE_SECS`. The check suite shortens it with
    /// `HX_AR_WATCHDOG_SECS`, since a cap is at least a minute.
    fn watchdog(&self) -> std::time::Duration {
        let secs = std::env::var("HX_AR_WATCHDOG_SECS").ok().and_then(|v| v.parse::<f64>().ok()).filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or(self.spec().episode_secs + GRACE_SECS);
        std::time::Duration::from_secs_f64(secs)
    }

    fn run_name(&self) -> String { self.dir.file_name().unwrap_or_default().to_string_lossy().into_owned() }

    /// One fresh child run in its own copy of the base holding its starting state, served over its channel until it
    /// closes it. The attempt is journaled before anything else happens, so a resume always knows it was started.
    async fn episode(&self, plan: &Plan, attempt: usize, task: String) -> Result<Ran> {
        use std::os::fd::AsRawFd;
        use tokio::io::AsyncReadExt;
        let label = attempt_label(&plan.label, attempt);
        ensure!(valid_label(&label), "invalid episode label {label:?} in the journal");
        let ep_dir = self.dir.join("episodes");
        std::fs::create_dir_all(&ep_dir)?;
        let task_path = ep_dir.join(format!("{label}.md"));
        std::fs::write(&task_path, task)?;
        self.journal(Record::Episode { plan: plan.label.clone(), attempt, role: plan.role, outcome: Outcome::Started, report_ref: None })?;
        let scratch = Scratch::episode(&self.run_name(), &label)?;
        let ws = scratch.0.join("ws");
        copy_tree(&self.dir.join("base"), &ws)?;
        place(&ws, &self.spec().editable, &load_snap(&self.dir, &plan.start)?)?;
        // The child's temp directory, its policy's private one included, and its journal are inside its copy: whatever
        // a killed child leaves there is deleted with the copy.
        let (tmp, log) = (scratch.0.join("tmp"), scratch.0.join("log"));
        std::fs::create_dir(&tmp)?;
        std::fs::create_dir(&log)?;
        let episode = Episode { label: label.clone(), role: plan.role, editable: self.spec().editable.clone(),
            sealed: self.sealed()?, log: log.clone() };
        eprintln!("hx ar: {label} ({:?}{})", plan.role, if plan.candidate > 0 { format!(" #{}", plan.candidate) } else { String::new() });
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        let fd = theirs.as_raw_fd();
        let mut command = tokio::process::Command::new(&self.exe);
        command.arg("run").arg("--workspace").arg(&ws).args(&self.launch.child_args)
            .arg("--prompt-file").arg(&task_path)
            .arg("--max-turns").arg(TURNS.to_string())
            .arg("--max-wall").arg(self.spec().episode_secs.to_string())
            .env("HX_AR_EPISODE", serde_json::to_string(&episode)?).env("TMPDIR", &tmp)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        // The child's end of the channel on `episode::FD`, the only descriptor it inherits beyond the standard three.
        unsafe {
            command.pre_exec(move || {
                let ok = if fd == episode::FD { libc::fcntl(fd, libc::F_SETFD, 0) } else { libc::dup2(fd, episode::FD) };
                if ok < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
            });
        }
        let mut child = command.spawn()?;
        drop(theirs);
        ours.set_nonblocking(true)?;
        let channel = tokio::net::UnixStream::from_std(ours)?;
        let _tracked = child.id().map(crate::prims::Child::track);
        let drain = |pipe: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>| tokio::spawn(async move {
            let mut buf = vec![];
            if let Some(mut pipe) = pipe { let _ = pipe.read_to_end(&mut buf).await; }
            buf
        });
        let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
        let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
        let wait = async {
            match tokio::time::timeout(self.watchdog(), child.wait()).await {
                Ok(status) => status.map(|_| false),
                Err(_) => {
                    eprintln!("hx ar: {label} outlived its {}s cap; killing it", self.spec().episode_secs);
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    Ok(true)
                }
            }
        };
        let (_, timed_out) = futures::future::join(self.serve(plan, &label, channel), wait).await;
        let timed_out = timed_out?;
        // A process the child left holding its pipes must not hold the round either.
        let collect = |h: tokio::task::JoinHandle<Vec<u8>>| async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), h).await.ok().and_then(Result::ok).unwrap_or_default()
        };
        let (stdout, stderr) = (collect(stdout).await, collect(stderr).await);
        std::fs::write(ep_dir.join(format!("{label}.stderr")), &stderr)?;
        let report = String::from_utf8_lossy(&stdout).lines().rev()
            .find_map(|l| serde_json::from_str::<Value>(l).ok()).unwrap_or(Value::Null);
        self.keep_log(&scratch.0, &label);
        drop(scratch);
        let evals = self.book().evals.iter().filter(|e| e.1 == label).count();
        let outcome = if timed_out { Outcome::TimedOut } else if evals == 0 && report.is_null() { Outcome::Crashed } else { Outcome::Done };
        self.journal(Record::Episode { plan: plan.label.clone(), attempt, role: plan.role, outcome, report_ref: self.blob(&report)? })?;
        let end = if timed_out { "timed out" } else if report["finished"] == true { "finished" } else { report["stopped"].as_str().unwrap_or("stopped") };
        eprintln!("hx ar: {label}: {evals} evals, {end}");
        Ok(Ran { crashed: outcome == Outcome::Crashed, stderr: String::from_utf8_lossy(&stderr).into_owned() })
    }

    /// Keep a child's journal with the run (`episodes/LABEL.log`) before its copy goes.
    fn keep_log(&self, copy: &Path, label: &str) {
        let to = self.dir.join("episodes").join(format!("{label}.log"));
        if copy.join("log").is_dir() && !to.exists() && copy_tree(&copy.join("log"), &to).is_err() {
            eprintln!("hx ar: could not keep the journal of {label}");
        }
    }

    /// What a child's policy seals: this workspace's runs and both evaluators' roots (`sealed`).
    fn sealed(&self) -> Result<Vec<PathBuf>> {
        Ok(self.dir.parent().map(Path::to_path_buf).into_iter().chain(self.spec().roots(true)?).collect())
    }

    /// Serve one child's requests until it closes its end, each in turn. An answer the child is no longer there to
    /// read is dropped; what was journaled stays.
    async fn serve(&self, plan: &Plan, label: &str, channel: tokio::net::UnixStream) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (input, mut output) = channel.into_split();
        let mut lines = tokio::io::BufReader::new(input).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let reply = match self.answer_request(plan, label, &req).await {
                Ok(result) => json!({"id": req["id"], "ok": true, "result": result}),
                Err(e) => json!({"id": req["id"], "ok": false, "error": e.to_string()}),
            };
            if output.write_all((reply.to_string() + "\n").as_bytes()).await.is_err() { break; }
        }
    }

    async fn answer_request(&self, plan: &Plan, label: &str, req: &Value) -> Result<Value> {
        match req["op"].as_str() {
            Some("eval") => self.eval(plan, label, req["note"].as_str().unwrap_or(""), &from_wire(&self.spec().editable, &req["files"])?).await,
            Some("reset") => {
                let (best, at) = progress(self.spec(), &self.book(), plan, label);
                Ok(json!({"files": to_wire(&load_snap(&self.dir, at.as_deref().unwrap_or(&plan.start))?), "best": best}))
            }
            Some("verdict") => self.verdict(plan, req["pass"].as_bool().context("pass must be true or false")?, req["reason"].as_str().unwrap_or("")),
            _ => bail!("unknown request"),
        }
    }

    /// `ar.eval`: store the snapshot, score it once, and make it a state of the worker's candidate (a probe for research
    /// and audits). A state the candidate already has, or that a probe finds already recorded, keeps its row: the answer
    /// is that row, `cached`.
    async fn eval(&self, plan: &Plan, label: &str, note: &str, files: &Files) -> Result<Value> {
        let snap = save_snap(&self.log, files)?;
        let score = self.score(Tier::Search, &snap).await?;
        let candidate = if matches!(plan.role, Role::Explore | Role::Exploit) { plan.candidate } else { 0 };
        let before = progress(self.spec(), &self.book(), plan, label);
        let mut n = 0;
        let row = self.journal_if(|records| {
            let mine = |r: &&Record| matches!(r, Record::Eval { snap: s, candidate: c, .. } if *s == snap && (candidate == 0 || *c == candidate));
            if let Some(Record::Eval { n: old, .. }) = records.iter().find(mine) { n = *old; return None; }
            n = records.iter().filter(|r| matches!(r, Record::Eval { .. })).count() + 1;
            Some(Record::Eval { n, episode: label.into(), candidate, snap: snap.clone(), note: note.into() })
        })?;
        let (best, at) = progress(self.spec(), &self.book(), plan, label);
        let mut out = json!({"n": n, "score": score.score, "valid": score.valid, "se": score.se, "improved": at != before.1 && at.is_some(),
            "best": if candidate == 0 { None } else { best }, "report": self.read(score.report.as_deref())});
        if row.is_none() { out["cached"] = json!(true); }
        Ok(out)
    }

    /// `ar.verdict`: an auditor's verdict on the state it audits, journaled once.
    fn verdict(&self, plan: &Plan, pass: bool, reason: &str) -> Result<Value> {
        ensure!(plan.role == Role::Audit, "ar.verdict is for auditors");
        let reason: String = reason.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(1_000).collect();
        ensure!(pass || !reason.is_empty(), "a failing verdict needs its reason: ar.verdict{{pass = false, reason = \"...\"}}");
        let recorded = self.journal_if(|records| (!records.iter().any(|r| matches!(r, Record::Verdict { snap, .. } if *snap == plan.start)))
            .then(|| Record::Verdict { snap: plan.start.clone(), pass, reason: reason.clone() }))?;
        ensure!(recorded.is_some(), "this auditor already gave its verdict, and a verdict is final");
        eprintln!("hx ar: audit of #{}: {}{}", plan.candidate, if pass { "pass" } else { "FAIL" }, if reason.is_empty() { String::new() } else { format!(" ({reason})") });
        Ok(json!({"ok": true, "message": "Verdict recorded; it is final. Finish with your final answer."}))
    }

    /// The brief and the findings so far, as episodes and idea calls are shown them.
    fn view(&self) -> View {
        let records = self.records();
        let book = Book::of(&records);
        let brief = self.answer(&records, "r0-research").unwrap_or_else(|| "(none: the research episode wrote no brief)".into());
        // A finding per worker attempt that did not crash: its idea, best score and final answer; the most recent are shown.
        let mut findings = vec![];
        for r in &records {
            let Record::Episode { plan, attempt, outcome, report_ref, .. } = r else { continue };
            let Some(plan) = book.plans.get(plan).filter(|p| matches!(p.role, Role::Explore | Role::Exploit)) else { continue };
            if matches!(outcome, Outcome::Started | Outcome::Crashed) { continue; }
            let label = attempt_label(&plan.label, *attempt);
            let idea = plan.idea.clone().unwrap_or_default().replace('\n', " ");
            let what = match plan.role { Role::Explore => format!("idea #{}: {idea}", plan.candidate), _ => format!("refinement of #{}: {idea}", plan.candidate) };
            let best = book.evals.iter().filter(|e| e.1 == label).filter_map(|e| book.search(&e.3))
                .fold(None, |b: Option<f64>, s| if b.is_none_or(|b| self.spec().ahead(s, b)) { Some(s) } else { b });
            let text = self.read(report_ref.as_deref())["final_answer"].as_str().unwrap_or_default().to_string();
            findings.push(format!("### {label}: {what} (best {})\n\n{}\n\n", show(best), clip(text.trim(), FINDING_CHARS)));
        }
        let skip = findings.len().saturating_sub(FINDINGS);
        let findings = if skip > 0 { format!("({skip} earlier findings omitted)\n\n") } else { String::new() } + &findings[skip..].concat();
        View { book, brief, findings }
    }

    /// The tools an episode has, in the order the prompt guide asks for: grounding before the task, rules last.
    fn tools(&self, role: Role) -> String {
        let spec = self.spec();
        let mut flags = vec![if matches!(role, Role::Explore | Role::Exploit) { "worker" } else { "probe" }];
        if role == Role::Audit { flags.push("audit"); }
        if spec.select.is_some() { flags.push("select"); }
        fill(TOOLS, &[
            ("editable", spec.editable.join(", ")),
            ("better", if spec.direction == Direction::Max { "higher" } else { "lower" }.into()),
            ("margin", format!("min_delta {}, or the evaluator's `se` when that is larger", spec.min_delta)),
            ("turns", TURNS.to_string()),
            ("minutes", (spec.episode_secs / 60.0).round().to_string()),
        ], &flags) + "\n"
    }

    /// What an episode, or the idea call that plans it, is shown before its role: the program, the brief and findings,
    /// the state it starts from, and for a refinement the candidate's evaluations and the checks that rejected some.
    fn context(&self, plan: &Plan, view: &View) -> String {
        let book = &view.book;
        let start = book.score(Tier::Search, &plan.start);
        let report = clip(&serde_json::to_string_pretty(&start.map_or(Value::Null, |s| self.read(s.report.as_deref()))).unwrap_or_default(), REPORT_CHARS);
        let mut context = format!("{}\n\n", self.launch.program.trim_end());
        if plan.role != Role::Research {
            context += &format!("<brief>\n{}\n</brief>\n\n", clip(&view.brief, BRIEF_CHARS));
            if !view.findings.is_empty() { context += &format!("<findings>\n{}</findings>\n\n", view.findings); }
        }
        context += &format!("<starting_state>\nYour workspace holds it (search score {}). Its evaluator report:\n```json\n{report}\n```\n</starting_state>\n\n",
            show(start.and_then(Score::value)));
        if plan.role == Role::Exploit {
            let rows: Vec<_> = book.evals.iter().filter(|e| e.2 == plan.candidate).collect();
            let skip = rows.len().saturating_sub(SHOWN);
            context += &format!("<ledger>\nn | score | worker | note\n{}", if skip > 0 { format!("({skip} older rows omitted)\n") } else { String::new() });
            for (n, episode, _, snap, note) in &rows[skip..] {
                let score = book.search(snap).map_or("invalid".into(), |s| format!("{s:.6}"));
                context += &format!("{n} | {score} | {episode} | {}\n", note.replace('\n', " "));
            }
            context += "</ledger>\n\n";
            context += &rejections(self.spec(), book, plan.candidate);
        }
        context
    }

    fn vars(&self, plan: &Plan, label: &str, book: &Book) -> Vec<(&'static str, String)> {
        let spec = self.spec();
        vec![("label", label.to_string()), ("workers", spec.workers.to_string()), ("editable", spec.editable.join(", ")),
            ("candidate", plan.candidate.to_string()), ("candidate_idea", book.idea(plan.candidate)), ("idea", plan.idea.clone().unwrap_or_default()),
            ("best", show(plan.best))]
    }

    fn task(&self, plan: &Plan, label: &str, view: &View) -> Result<String> {
        let (context, tools, vars) = (self.context(plan, view), self.tools(plan.role), self.vars(plan, label, &view.book));
        Ok(format!("{context}{tools}{}", fill(match plan.role { Role::Research => RESEARCH, Role::Explore => EXPLORE, Role::Exploit => EXPLOIT,
            Role::Audit => AUDIT }, &vars, &[])))
    }

    /// The host the orchestrator's own model calls go through: the episodes' model settings, admitted and journaled in
    /// the run's journal like any run's calls.
    fn planner(&self) -> Result<Arc<crate::host::Host>> {
        let mut slot = self.planner.lock().unwrap();
        if let Some(host) = slot.as_ref() { return Ok(host.clone()); }
        let args: Vec<String> = ["run".to_string()].into_iter().chain(self.launch.child_args.iter().cloned()).collect();
        let llm = crate::llm_cfg(&args, self.log.session.clone())?;
        let policy = Arc::new(crate::policy::Policy::journal_only(&self.workspace, self.log.clone()));
        let host = crate::host::Host::calls_only(llm, policy)?;
        *slot = Some(host.clone());
        Ok(host)
    }

    /// One idea per plan in `plans` (all of one kind: explore ideas, or refinements of one candidate from one state).
    /// Each is sampled with String Seed of Thought from its own OS seed, all side by side; one dedupe call then compares
    /// them with each other and with `taken`. A duplicate is sampled again, with everything accepted so far listed as
    /// taken, at most `RESAMPLES` times; then it is accepted as it is. A sample that fails is sampled again the same way.
    async fn ideas(&self, plans: &mut [Plan], view: &View, taken: &[String]) -> Result<()> {
        let host = self.planner()?;
        let mut accepted: Vec<String> = taken.to_vec();
        let mut todo: Vec<usize> = (0..plans.len()).collect();
        let mut error = String::new();
        for pass in 0..=RESAMPLES {
            let asks = todo.iter().map(|i| -> Result<Value> {
                let plan = &plans[*i];
                let listed: String = accepted.iter().map(|t| format!("- {t}\n")).collect();
                let mut vars = self.vars(plan, &plan.label, &view.book);
                vars.extend([("seed", ssot::seed()?), ("taken", if listed.is_empty() { String::new() } else { listed })]);
                let flags: &[&str] = if plan.role == Role::Explore { &["explore"] } else { &["exploit"] };
                let flags = if accepted.is_empty() { flags.to_vec() } else { [flags, &["taken"]].concat() };
                Ok(json!({"system": "You propose research ideas and answer exactly in the format asked.",
                    "prompt": self.context(plan, view) + &fill(IDEA, &vars, &flags), "max_tokens": 4000}))
            }).collect::<Result<Vec<_>>>()?;
            let replies = host.complete(Value::Array(asks)).await?;
            let mut fresh = vec![];
            for (i, reply) in todo.iter().zip(replies.as_array().into_iter().flatten()) {
                match reply["value"].as_str().and_then(idea_of) {
                    Some(idea) => { plans[*i].idea = Some(idea); fresh.push(*i); }
                    None => error = reply["error"].as_str().unwrap_or("an empty answer").to_string(),
                }
            }
            let dupes = if pass == RESAMPLES { vec![] } else { self.duplicates(&host, plans[0].role, &accepted, &fresh.iter().map(|i| plans[*i].idea.clone().unwrap_or_default()).collect::<Vec<_>>()).await };
            let again: Vec<usize> = todo.iter().copied().filter(|i| !fresh.contains(i) || dupes.contains(&(fresh.iter().position(|f| f == i).unwrap() + 1))).collect();
            accepted.extend(fresh.iter().filter(|i| !again.contains(i)).filter_map(|i| plans[*i].idea.clone()));
            if again.is_empty() { return Ok(()); }
            if pass < RESAMPLES { eprintln!("hx ar: sampling {} idea(s) again: {}", again.len(), if error.is_empty() { "duplicates" } else { &error }); }
            todo = again;
        }
        let missing: Vec<&str> = plans.iter().filter(|p| p.idea.is_none()).map(|p| p.label.as_str()).collect();
        ensure!(missing.is_empty(), "could not plan an idea for {}: {error}", missing.join(", "));
        Ok(())
    }

    /// The new ideas (numbered from 1) that one dedupe call finds the same as a taken one or an earlier new one. A call
    /// that fails or cannot be read finds none: a borderline idea gets the benefit of the doubt.
    async fn duplicates(&self, host: &crate::host::Host, role: Role, taken: &[String], new: &[String]) -> Vec<usize> {
        if new.is_empty() || new.len() == 1 && taken.is_empty() { return vec![]; }
        let list = |items: &[String], prefix: &str| -> String { items.iter().enumerate().map(|(i, t)| format!("{prefix}{}. {}\n", i + 1, t.replace('\n', " "))).collect() };
        let flags: &[&str] = if role == Role::Explore { &["explore"] } else { &["exploit"] };
        let prompt = fill(DEDUPE, &[("taken", if taken.is_empty() { "(none)\n".into() } else { list(taken, "T") }), ("new", list(new, ""))], flags);
        let request = json!({"system": "You compare research ideas and answer exactly in the format asked.", "prompt": prompt, "effort": "none", "max_tokens": 2000});
        let Ok(replies) = host.complete(json!([request])).await else { return vec![] };
        replies[0]["value"].as_str().map(|t| duplicates_of(t, new.len())).unwrap_or_default()
    }

    /// The plans of round `round`: explore workers each with a planned idea that founds a new candidate, or exploit
    /// workers split across the chosen candidates, each with a refinement planned per candidate and starting state.
    async fn round_plans(&self, round: usize, explore: bool) -> Result<Vec<Plan>> {
        let (view, workers, records) = (self.view(), self.spec().workers, self.records());
        let book = &view.book;
        if explore {
            let next = book.candidates.iter().map(|c| c.0).max().unwrap_or(0);
            let mut plans: Vec<Plan> = (1..=workers).map(|w| Plan { label: format!("r{round}-w{w}"), role: Role::Explore, round,
                candidate: next + w as u32, start: book.seed.clone(), best: None, idea: None }).collect();
            // Ideas earlier rounds planned are taken while their candidate holds a valid state; one whose worker never
            // scored one is free again.
            let taken: Vec<String> = book.candidates.iter().filter(|(id, _)| book.evals.iter().any(|e| e.2 == *id && book.search(&e.3).is_some()))
                .map(|c| c.1.clone()).collect();
            self.ideas(&mut plans, &view, &taken).await?;
            return Ok(plans);
        }
        let (decisions, chosen) = self.gated(None).unwrap_or_default();
        let mut plans: Vec<Plan> = (0..workers).map(|w| {
            let c = decisions.iter().find(|c| c.candidate == chosen[w % chosen.len()]).expect("chosen candidates are listed");
            Plan { label: format!("r{round}-w{}", w + 1), role: Role::Exploit, round, candidate: c.candidate, start: c.frontier.snap.clone(),
                best: Some(c.frontier.search), idea: None }
        }).collect();
        let (mut groups, view) = (vec![], &view);
        for c in &chosen {
            let mut group: Vec<Plan> = plans.iter().filter(|p| p.candidate == *c).cloned().collect();
            if group.is_empty() { continue; }
            // Refinements tried from the same state are taken while they produced a valid state.
            let taken: Vec<String> = book.plans.values().filter(|p| p.role == Role::Exploit && p.candidate == *c && p.start == group[0].start)
                .filter(|p| attempts(&records, &p.label).iter().any(|a| book.evals.iter().any(|e| e.1 == a.2 && book.search(&e.3).is_some())))
                .filter_map(|p| p.idea.clone()).collect();
            groups.push(async move { self.ideas(&mut group, &view, &taken).await.map(|_| group) });
        }
        for group in futures::future::join_all(groups).await {
            for p in group? { if let Some(slot) = plans.iter_mut().find(|q| q.label == p.label) { *slot = p; } }
        }
        Ok(plans)
    }

    /// The gate after `round`, or the run's last gate: audit what `audits` picks, in one batch, then journal the
    /// `TOP` candidates the next round refines.
    async fn gate(&self, round: usize, last: bool) -> Result<Vec<u32>> {
        let batch = if last { "audit-last".to_string() } else { format!("audit{round}") };
        // A resumed run finishes the batch it planned, so verdicts come from that batch's own plans.
        let plans = match self.planned(&batch) {
            Some(plans) => plans,
            None => {
                let gated = self.decide().await?;
                let plans: Vec<Plan> = audits(self.spec(), &gated, last).into_iter()
                    .map(|d| Plan { label: format!("{}{round}-c{}", if last { "f" } else { "a" }, d.candidate), role: Role::Audit, round,
                        candidate: d.candidate, start: d.frontier.snap.clone(), best: None, idea: None }).collect();
                if !plans.is_empty() { self.plan(&batch, plans.clone())?; }
                plans
            }
        };
        if !plans.is_empty() {
            self.run_batch(&batch).await?;
            let book = self.book();
            for plan in plans.iter().filter(|p| !book.verdicts.contains_key(&p.start)) {
                eprintln!("hx ar: audit of #{}: no verdict; it stays unaudited", plan.candidate);
            }
        }
        let gated = self.decide().await?;
        let chosen: Vec<u32> = gated.ranked.iter().take(TOP).copied().collect();
        eprintln!("hx ar: {} {round}: {} candidates, {} good, {} with a result, going on with {:?}", if last { "last gate after round" } else { "gate" },
            gated.decisions.len(), gated.decisions.iter().filter(|c| c.good).count(), gated.decisions.iter().filter(|c| c.result.is_some()).count(), chosen);
        self.journal(Record::Gate { round, last, chosen: chosen.clone() })?;
        Ok(chosen)
    }

    /// Close the attempts a stopped orchestrator left open, durably, keeping their journals and deleting their copies
    /// (the child's temp directory with them).
    fn close(&self) -> Result<()> {
        let records = self.records();
        let mut open: Vec<(String, usize, Role)> = vec![];
        for r in &records {
            let Record::Episode { plan, attempt, role, outcome, .. } = r else { continue };
            if *outcome == Outcome::Started { open.push((plan.clone(), *attempt, *role)); } else { open.retain(|o| (&o.0, o.1) != (plan, *attempt)); }
        }
        for (plan, attempt, role) in open {
            let label = attempt_label(&plan, attempt);
            ensure!(valid_label(&label), "invalid episode label {label:?} in the journal");
            for copy in left_copies(&self.run_name(), &label) {
                self.keep_log(&copy, &label);
                drop(Scratch(copy));
            }
            self.journal(Record::Episode { plan, attempt, role, outcome: Outcome::Interrupted, report_ref: None })?;
        }
        Ok(())
    }

    async fn run(&self) -> Result<Value> {
        self.close()?;
        let seed = self.launch.seed.clone();
        // A seed the search evaluator cannot score leaves nothing to beat; a select evaluator that cannot score it would
        // veto every candidate. Either is a broken setup: stop before spending.
        let baseline = self.score(Tier::Search, &seed).await?;
        eprintln!("hx ar: seed scores {}", if baseline.valid { show(baseline.score) } else { "invalid".into() });
        ensure!(baseline.value().is_some(), "the search evaluator gives no valid score for the seed; fix it before searching. Its report: {}",
            clip(&self.read(baseline.report.as_deref()).to_string(), 2_000));
        if self.spec().select.is_some() {
            ensure!(self.score(Tier::Select, &seed).await?.value().is_some(), "the select evaluator gives no valid score for the seed; fix it before searching");
        }
        self.plan("research", vec![Plan { label: "r0-research".into(), role: Role::Research, round: 0, candidate: 0, start: seed.clone(), best: None, idea: None }])?;
        self.run_batch("research").await?;
        // Round 1 explores. A round makes progress when it chooses a new good candidate or a chosen one beats its score
        // at the last progress by the margin (`progressed`), so small steps add up across rounds until they count.
        // After exploring the run refines; it refines again while that moves a chosen frontier at all, and explores
        // again after a refining round that moved none. Two rounds in a row without progress stop the run, and
        // `max_rounds` ends it whatever the scores do.
        let (mut at, mut prev) = (BTreeMap::<u32, (f64, Option<f64>)>::new(), Vec::<(u32, String)>::new());
        let (mut explore, mut quiet, mut rounds) = (true, false, 0);
        for round in 1..=self.spec().max_rounds {
            let batch = format!("round{round}");
            let mut gate = self.gated(Some(round));
            let plans = match self.planned(&batch) {
                Some(plans) => plans,
                None if gate.is_some() => vec![],
                None => {
                    let plans = self.round_plans(round, explore).await?;
                    self.plan(&batch, plans.clone())?;
                    plans
                }
            };
            if gate.is_none() {
                self.run_batch(&batch).await?;
                self.gate(round, false).await?;
                gate = self.gated(Some(round));
            }
            rounds = round;
            let (decisions, chosen) = gate.unwrap_or_default();
            // With no candidate after a gate the run stops (in practice after round 1: later gates keep earlier candidates).
            if chosen.is_empty() { break; }
            let chosen: Vec<&Decision> = chosen.iter().filter_map(|id| decisions.iter().find(|c| c.candidate == *id)).collect();
            let progress = progressed(self.spec(), &mut at, &chosen);
            let moved = chosen.iter().any(|c| !prev.contains(&(c.candidate, c.frontier.snap.clone())));
            if !progress && quiet {
                eprintln!("hx ar: two rounds in a row made no progress; stopping");
                break;
            }
            if round == self.spec().max_rounds { eprintln!("hx ar: {round} rounds, the most this run takes; stopping"); }
            quiet = !progress;
            explore = !moved && plans.first().is_some_and(|p| p.role == Role::Exploit);
            prev = chosen.iter().map(|c| (c.candidate, c.frontier.snap.clone())).collect();
        }
        ensure!(!self.book().evals.is_empty(), "autoresearch made no evaluations; see {}", self.dir.join("episodes").display());
        if !self.records().iter().any(|r| matches!(r, Record::Gate { last: true, .. })) { self.gate(rounds, true).await?; }
        self.finish()
    }

    /// Write the result from the last journaled gate, starting nothing: the portfolio's states into `portfolio/c<id>/`
    /// and the best into the workspace's editable paths. When no candidate has a result the workspace is not touched.
    fn finish(&self) -> Result<Value> {
        let (records, book) = (self.records(), self.book());
        ensure!(!book.evals.is_empty(), "autoresearch made no evaluations; see {}", self.dir.join("episodes").display());
        let (decisions, chosen) = self.gated(None).context("this autoresearch run has no gate yet: nothing to finish")?;
        let portfolio = portfolio(self.spec(), &decisions);
        let editable = &self.spec().editable;
        // Each written whole or not at all: placed in a temporary directory and renamed, so an existing one is complete.
        for p in &portfolio {
            let dir = self.dir.join("portfolio").join(format!("c{}", p["candidate"]));
            if dir.exists() { continue; }
            let tmp = self.dir.join("portfolio").join(format!(".c{}.tmp", p["candidate"]));
            let _ = std::fs::remove_dir_all(&tmp);
            place(&tmp, editable, &load_snap(&self.dir, p["snap"].as_str().unwrap())?)?;
            std::fs::rename(&tmp, &dir)?;
        }
        let (snap, search, select) = match portfolio.first() {
            Some(p) => {
                place(&self.workspace, editable, &load_snap(&self.dir, p["snap"].as_str().unwrap())?)?;
                (p["snap"].as_str().unwrap().to_string(), p["search"].as_f64(), p["select"].as_f64())
            }
            None => {
                eprintln!("hx ar: no candidate beat the seed; the workspace is unchanged");
                (book.seed.clone(), book.search(&book.seed), book.select(&book.seed).flatten())
            }
        };
        let report = json!({
            "run": self.run_name(), "dir": self.dir, "commit": self.launch.commit,
            "snap": snap, "search": search, "select": select, "portfolio": portfolio, "candidates": decisions, "chosen": chosen,
            "rounds": records.iter().filter(|r| matches!(r, Record::Gate { last: false, .. })).count(),
            "evals": book.evals.len(),
        });
        crate::atomic_file::replace_trusted(&self.dir.join("report.json"), &serde_json::to_vec_pretty(&report)?)?;
        Ok(report)
    }
}

/// The checks that rejected states of a candidate, with their reasons, so its next workers (and the calls that plan
/// their refinements) do not bring back a hole an auditor found or a change the held-out data refused. The held-out
/// score itself is never shown.
fn rejections(spec: &Spec, book: &Book, candidate: u32) -> String {
    let mut out = String::new();
    let mut seen = HashSet::new();
    for (n, _, _, snap, _) in book.evals.iter().filter(|e| e.2 == candidate && book.search(&e.3).is_some() && seen.insert(e.3.clone())) {
        let score = show(book.search(snap));
        if let Some((false, reason)) = book.verdicts.get(snap) {
            out += &format!("- State n={n} (score {score}) failed its audit: {}\n", if reason.is_empty() { "no reason given" } else { reason });
        }
        if spec.select.is_some() && book.select(snap).is_some_and(|s| book.vetoes(spec, s)) {
            out += &format!("- State n={n} (score {score}) was vetoed: it did worse than the starting point on held-out data no agent sees, so it does not generalize.\n");
        }
    }
    if out.is_empty() { out } else { format!("<checks>\nThese states of this candidate were rejected; the candidate fell back to its best state no check rejected:\n{out}</checks>\n\n") }
}

/// What the journal says, as episodes are shown it.
struct View {
    book: Book,
    brief: String,
    findings: String,
}

/// An idea from a planning call's answer: the text after its last `IDEA:` line, or the whole answer without one, on one
/// line and at most 1,000 characters. None when empty.
fn idea_of(text: &str) -> Option<String> {
    let text = text.rfind("IDEA:").map_or(text, |at| &text[at + "IDEA:".len()..]);
    let idea: String = text.split_whitespace().collect::<Vec<_>>().join(" ").trim_matches(['*', '`', ' ']).chars().take(1_000).collect();
    (!idea.is_empty()).then_some(idea)
}

/// The numbers (1..=n) a dedupe call's last `Duplicates:` (or `Answer:`) line names; `none` or 0 is none.
fn duplicates_of(text: &str, n: usize) -> Vec<usize> {
    let lower = text.to_ascii_lowercase();
    let Some(at) = lower.rfind("duplicates").or_else(|| lower.rfind("answer")) else { return vec![] };
    let line = lower[at..].lines().next().unwrap_or("");
    line.split(|c: char| !c.is_ascii_digit()).filter_map(|d| d.parse::<usize>().ok()).filter(|d| (1..=n).contains(d)).collect()
}

/// Did this gate make progress? A chosen candidate makes it when it is good and new, or when its frontier beats its
/// score at the last progress by the margin. A candidate that is not good is remembered when first chosen, so its later
/// lifts count but its arrival does not.
fn progressed(spec: &Spec, at: &mut BTreeMap<u32, (f64, Option<f64>)>, chosen: &[&Decision]) -> bool {
    let progress = chosen.iter().any(|c| match at.get(&c.candidate) {
        None => c.good,
        Some(&(before, se)) => spec.better(c.frontier.search, Some(before), margin(spec, c.frontier.se, se)),
    });
    for c in chosen {
        if progress || !at.contains_key(&c.candidate) { at.insert(c.candidate, (c.frontier.search, c.frontier.se)); }
    }
    progress
}

/// An episode label as the orchestrator makes them (`r1-w3`, `a2-c5`, a retry `r1-w3.2`): it names files in the run
/// and the episode's copy.
fn valid_label(label: &str) -> bool {
    let (name, retry) = label.split_once('.').unwrap_or((label, "1"));
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !retry.is_empty() && retry.bytes().all(|b| b.is_ascii_digit())
}

/// A role prompt from its template: a `[[flag]] ` line is kept only when `flag` is set, and each `{{name}}` is replaced
/// by its value in one pass, so a value that contains braces (an idea, a report) is never expanded again.
fn fill(template: &str, vars: &[(&str, String)], flags: &[&str]) -> String {
    let mut text = String::new();
    for line in template.lines() {
        let line = match line.strip_prefix("[[").and_then(|l| l.split_once("]] ")) {
            Some((flag, rest)) if flags.contains(&flag) => rest,
            Some(_) => continue,
            None => line,
        };
        text += line;
        text.push('\n');
    }
    let (mut out, mut rest) = (String::with_capacity(text.len()), text.as_str());
    while let Some(at) = rest.find("{{") {
        out += &rest[..at];
        let after = &rest[at + 2..];
        match after.find("}}").and_then(|end| vars.iter().find(|(k, _)| *k == &after[..end]).map(|(_, v)| (end, v))) {
            Some((end, value)) => { out += value; rest = &after[end + 2..]; }
            None => { out += "{{"; rest = after; }
        }
    }
    out + rest
}

/// At most `max` characters of `text`, marked when cut.
fn clip(text: &str, max: usize) -> String {
    let cut: String = text.chars().take(max).collect();
    if cut.len() < text.len() { cut + "\n... (truncated)" } else { cut }
}

fn show(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.6}")).unwrap_or_else(|| "none".into())
}

/// What the owner asked of `hx run --ar` beyond the program.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// `--ar-unconfined`: run although the kernel cannot confine episodes and evaluators.
    pub unconfined: bool,
    /// `--finish`: write a resumed run's result from its last gate and start nothing.
    pub finish: bool,
}

/// Without kernel confinement an episode's shell can read the grader and held-out roots and the run directory, so a
/// spec that has any of them refuses to run unless the owner acknowledges it. A spec with none has nothing to hide
/// beyond what the agent may see; it runs with the usual note.
fn confinement(spec: &Spec, status: Result<i32, &str>, acknowledged: bool) -> Result<()> {
    let Err(why) = status else { return Ok(()) };
    if spec.select.is_none() && spec.grader.is_empty() && spec.holdout.is_empty() { return Ok(()); }
    ensure!(acknowledged, "--ar cannot confine episodes and evaluators here ({why}): their shell commands could read the \
grader and held-out data and the run's records. Run hx in a sandbox of your own and pass --ar-unconfined to go ahead");
    eprintln!("hx ar: running unconfined ({why}), as --ar-unconfined allows: the grader and held-out data are only as safe as your own sandbox");
    Ok(())
}

/// The hx binary episodes run: a copy of this process's own executable in the run directory, made when the orchestrator
/// starts. Rebuilding hx replaces the file at its path (the old one is unlinked), so spawning that path would change
/// later episodes mid-run, and spawning `/proc/self/exe` would start children whose own executable is deleted (a child
/// reads it to record its hash). On Linux the copy is read from `/proc/self/exe`, so even a rebuild that lands during
/// the copy cannot change it.
fn hx_exe(dir: &Path) -> Result<PathBuf> {
    let proc = Path::new("/proc/self/exe");
    let from = if cfg!(target_os = "linux") && proc.exists() { proc.to_path_buf() } else { std::env::current_exe()? };
    let (to, tmp) = (dir.join("hx"), dir.join(".hx.tmp"));
    std::fs::copy(&from, &tmp).with_context(|| format!("copying the hx binary into {}", dir.display()))?;
    std::fs::rename(&tmp, &to)?;
    Ok(to)
}

/// The run's journal, opened for writing. Its writer lock is the run's: a second orchestrator on a live run would take
/// its episodes for interrupted ones and delete their copies.
fn open_journal(dir: &Path) -> Result<Arc<EventLog>> {
    let path = dir.join("journal.jsonl");
    if !path.exists() { std::fs::write(&path, "")?; }
    EventLog::reopen(&path).map_err(|e| if e.to_string().contains("already open by another process") {
        anyhow::anyhow!("autoresearch run {} is in use by another hx; wait for it to stop", dir.display())
    } else { e })
}

/// `hx run --ar`: search until two rounds in a row make no progress, or continue the run in `resume` (a run directory).
pub async fn orchestrate(workspace: &Path, start: Option<(Vec<String>, String)>, resume: Option<PathBuf>, options: Options) -> Result<()> {
    // A resumed run uses the spec frozen in its launch record, approved when it started; only a launch reads ar.toml.
    // The run directory is in the state root, which no agent can reach (file tools are sealed from it, shell commands
    // neither see nor write it), as safe as the trust store. A directory elsewhere (a run from before the state root, in
    // `.harness/ar`) is refused: shell commands could have rewritten it.
    let (dir, launch, log, records, resumed) = match (start, resume) {
        (_, Some(dir)) => {
            let dir = dir.canonicalize().with_context(|| format!("autoresearch run {}", dir.display()))?;
            let runs = crate::state::ar_dir(workspace)?;
            ensure!(dir.parent() == Some(runs.as_path()), "--resume for --ar takes one of this workspace's runs in {} (see `hx runs`); \
a run kept in the workspace's .harness/ar predates that and cannot be resumed", runs.display());
            let older = || anyhow::anyhow!("autoresearch run {} was started by an older hx, whose records this version reads \
differently; it cannot be resumed or finished here: start a new run", dir.display());
            if dir.join("launch.json").exists() || !dir.join("journal.jsonl").exists() { return Err(older()); }
            let log = open_journal(&dir)?;
            let records = load(&dir)?;
            let launch = match records.first() { Some(Record::Launch(l)) if l.format == FORMAT => l.clone(), _ => return Err(older()) };
            launch.spec.check(workspace)?;
            ensure!(!dir.join("report.json").exists(), "autoresearch run {} already finished", dir.display());
            if !options.finish { confinement(&launch.spec, crate::confine::status(), options.unconfined)?; }
            (dir, launch, log, records, true)
        }
        (Some((child_args, program)), None) => {
            ensure!(!options.finish, "--finish takes a run to finish: hx run --ar --resume RUN --finish");
            ensure!(crate::trust::is_trusted(workspace), "--ar runs the evaluator commands in .harness/ar.toml; review them and run `hx trust` first");
            ensure!(crate::workflow::git_inventory(workspace)?.is_some(), "--ar requires a Git repository at the workspace root");
            let spec = Spec::load(workspace)?;
            confinement(&spec, crate::confine::status(), options.unconfined)?;
            let bad = uncommitted(workspace, &spec)?;
            ensure!(bad.is_empty(), "--ar freezes the committed tree: commit or remove changes outside the editable paths: {}", bad.join(", "));
            // `.noindex`: Spotlight skips the run's snapshots, logs and copies.
            let id = format!("{}-{}.noindex", chrono_stamp(), std::process::id());
            let dir = crate::state::ar_dir(workspace)?.join(&id);
            std::fs::create_dir_all(&dir)?;
            let dir = dir.canonicalize()?;
            let log = open_journal(&dir)?;
            let commit = freeze(workspace, &dir)?;
            let seed = save_snap(&log, &capture(workspace, &spec.editable)?)?;
            let launch = Launch { format: FORMAT, spec, program, child_args, commit, seed };
            (dir, launch, log, vec![], false)
        }
        (None, None) => bail!("--ar needs a program, or --resume RUN_DIR"),
    };
    let exe = hx_exe(&dir)?;
    let o = Orchestrator::new(workspace, dir, launch.clone(), exe, log, records);
    if !resumed { o.journal(Record::Launch(launch))?; }
    let report = if options.finish {
        o.close()?;
        eprintln!("hx ar: finishing run {} from its last gate", o.dir.display());
        o.finish()?
    } else {
        eprintln!("hx ar: run {}{}", o.dir.display(), if resumed { ", resumed" } else { "" });
        o.run().await?
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episodes_get_a_wall_clock_cap() {
        let ws = std::env::temp_dir().join(format!("hx-ar-wall-{}", std::process::id()));
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        let write = |extra: &str| std::fs::write(ws.join(".harness/ar.toml"), format!("editable = [\"a.txt\"]\neval = \"true\"\n{extra}")).unwrap();
        write("");
        let spec = Spec::load(&ws).unwrap();
        assert_eq!((spec.episode_secs, spec.workers, spec.max_rounds, spec.direction), (900.0, 8, 20, Direction::Max));
        write("episode_secs = 300\nworkers = 3\nmax_rounds = 5\ndirection = \"min\"\n");
        let spec = Spec::load(&ws).unwrap();
        assert_eq!((spec.episode_secs, spec.workers, spec.max_rounds, spec.direction), (300.0, 3, 5, Direction::Min));
        for bad in ["episode_secs = 5\n", "workers = 0\n", "max_rounds = 0\n", "direction = \"up\"\n"] {
            write(bad);
            assert!(Spec::load(&ws).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-ar-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    fn spec(extra: &str) -> Spec {
        toml::from_str(&format!("editable = [\"x.txt\"]\neval = \"true\"\n{extra}")).unwrap()
    }

    /// An orchestrator over a fresh run directory in `state/ar/run` whose journal holds its launch (seed `seed`).
    fn orchestrator(tag: &str, spec: Spec) -> (PathBuf, Orchestrator) {
        let dir = temp(tag);
        let run = dir.join("state/ar/run");
        std::fs::create_dir_all(&run).unwrap();
        let launch = Launch { format: FORMAT, spec, program: "PROGRAM".into(), child_args: vec![], commit: String::new(), seed: "seed".into() };
        let o = Orchestrator::new(&dir.join("ws"), run.clone(), launch.clone(), PathBuf::new(), open_journal(&run).unwrap(), vec![]);
        o.journal(Record::Launch(launch)).unwrap();
        (dir, o)
    }

    #[test]
    fn one_margin_for_every_decision() {
        let s = spec("min_delta = 0.1\n");
        assert_eq!(margin(&s, None, None), 0.1);
        assert_eq!(margin(&s, Some(0.3), Some(0.4)), 0.5);
        assert_eq!(margin(&s, Some(0.05), None), 0.1);
        // Rounding is never better, whatever the margin.
        assert!(!spec("").better(1.0 + 1e-12, Some(1.0), 0.0) && spec("").better(1.1, Some(1.0), 0.0));
        let min = spec("direction = \"min\"\n");
        assert!(min.better(1.0, Some(2.0), 0.5) && !min.better(1.8, Some(2.0), 0.5));
    }

    fn launch() -> Record {
        Record::Launch(Launch { format: FORMAT, spec: spec(""), program: String::new(), child_args: vec![], commit: String::new(), seed: "seed".into() })
    }

    fn score(tier: Tier, snap: &str, score: Option<f64>, se: Option<f64>) -> Record {
        Record::Score { tier, snap: snap.into(), score, valid: score.is_some(), se, report_ref: None }
    }

    /// Records for candidates 1 to `n` planned in round 1, with the seed scoring 0 on both tiers (`seed_se` on search).
    fn base(n: u32, seed_se: Option<f64>) -> Vec<Record> {
        let plans = (1..=n).map(|c| Plan { label: format!("r1-w{c}"), role: Role::Explore, round: 1, candidate: c, start: "seed".into(),
            best: None, idea: Some(format!("idea {c}")) }).collect();
        vec![launch(), score(Tier::Search, "seed", Some(0.0), seed_se), score(Tier::Select, "seed", Some(0.0), None),
            Record::Planned { batch: "round1".into(), episodes: plans }]
    }

    /// A state of a candidate: its eval and its search score (`se`).
    fn state(n: usize, candidate: u32, snap: &str, search: f64, se: Option<f64>) -> Vec<Record> {
        vec![score(Tier::Search, snap, Some(search), se),
            Record::Eval { n, episode: format!("r1-w{candidate}"), candidate, snap: snap.into(), note: String::new() }]
    }

    fn verdict(snap: &str, pass: bool) -> Record { Record::Verdict { snap: snap.into(), pass, reason: String::new() } }

    /// The table: the seed scores 0 on search and 0 on select; select scores are given per state.
    struct Case {
        name: &'static str,
        spec: &'static str,
        seed_se: Option<f64>,
        states: Vec<(u32, &'static str, f64, Option<f64>)>,
        selects: Vec<(&'static str, Option<f64>)>,
        verdicts: Vec<(&'static str, bool)>,
        /// Per candidate: (frontier snap, good, result snap).
        want: Vec<(u32, &'static str, bool, Option<&'static str>)>,
        need_select: Vec<&'static str>,
        ranked: Vec<u32>,
    }

    fn records(case: &Case) -> Vec<Record> {
        let mut out = base(4, case.seed_se);
        for (i, (c, snap, search, se)) in case.states.iter().enumerate() { out.extend(state(i + 1, *c, snap, *search, *se)); }
        out.extend(case.selects.iter().map(|(s, v)| score(Tier::Select, s, *v, None)));
        out.extend(case.verdicts.iter().map(|(s, p)| verdict(s, *p)));
        out
    }

    #[test]
    fn the_gate_derives_frontiers_and_results() {
        let select = "select = \"s\"\n";
        let cases = vec![
            Case { name: "a select veto falls back like a failed audit (finding 1)", spec: select, seed_se: None,
                states: vec![(1, "x3", 3.0, None), (1, "x11", 11.0, None)], selects: vec![("x3", Some(1.0)), ("x11", Some(-5.0))],
                verdicts: vec![("x3", true)], want: vec![(1, "x3", true, Some("x3"))], need_select: vec![], ranked: vec![1] },
            Case { name: "an unaudited refinement keeps the passed result (finding 1)", spec: select, seed_se: None,
                states: vec![(1, "x3", 3.0, None), (1, "x4", 4.0, None)], selects: vec![("x3", Some(1.0)), ("x4", Some(1.0))],
                verdicts: vec![("x3", true)], want: vec![(1, "x4", true, Some("x3"))], need_select: vec![], ranked: vec![1] },
            Case { name: "a frontier that beats the seed needs its held-out score first", spec: select, seed_se: None,
                states: vec![(1, "x3", 3.0, None)], selects: vec![], verdicts: vec![],
                want: vec![(1, "x3", false, None)], need_select: vec!["x3"], ranked: vec![1] },
            Case { name: "the fallback is the best surviving state, improved or not (finding 8)", spec: "", seed_se: None,
                states: vec![(1, "x3", 3.0, None), (1, "x6", 6.0, None), (1, "x5", 5.0, None)], selects: vec![],
                verdicts: vec![("x6", false), ("x5", true)], want: vec![(1, "x5", true, Some("x5"))], need_select: vec![], ranked: vec![1] },
            Case { name: "only the seed's bytes are excluded; a tie is not good and ranks by search (finding 9)", spec: "", seed_se: None,
                states: vec![(1, "seed", 0.0, None), (1, "x5", 0.0, None), (2, "xm5", -1.0, None)], selects: vec![], verdicts: vec![],
                want: vec![(1, "x5", false, None), (2, "xm5", false, None)], need_select: vec![], ranked: vec![1, 2] },
            Case { name: "a lift within the evaluator's noise is not good (finding 10)", spec: "", seed_se: Some(0.5),
                states: vec![(1, "x01", 0.1, Some(0.5)), (2, "x2", 2.0, Some(0.1))],
                selects: vec![], verdicts: vec![("x01", true), ("x2", true)],
                want: vec![(1, "x01", false, None), (2, "x2", true, Some("x2"))], need_select: vec![], ranked: vec![2, 1] },
            Case { name: "min_delta is the bar against the seed", spec: "min_delta = 2\n", seed_se: None,
                states: vec![(1, "x1", 1.0, None), (2, "x3", 3.0, None)], selects: vec![], verdicts: vec![],
                want: vec![(1, "x1", false, None), (2, "x3", true, None)], need_select: vec![], ranked: vec![2, 1] },
            Case { name: "a candidate whose every state was rejected drops out; invalid select vetoes", spec: select, seed_se: None,
                states: vec![(1, "x3", 3.0, None), (2, "x4", 4.0, None)], selects: vec![("x3", None), ("x4", Some(0.0))],
                verdicts: vec![("x4", false)], want: vec![], need_select: vec![], ranked: vec![] },
            Case { name: "good ones rank first, then by search; a shared frontier goes on once", spec: "", seed_se: None,
                states: vec![(1, "x2", 2.0, None), (2, "x2", 2.0, None), (3, "xm1", -1.0, None), (4, "x1", 1.0, None)],
                selects: vec![], verdicts: vec![],
                want: vec![(1, "x2", true, None), (2, "x2", true, None), (3, "xm1", false, None), (4, "x1", true, None)],
                need_select: vec![], ranked: vec![1, 4, 3] },
        ];
        for case in cases {
            let gated = gate(&spec(case.spec), &records(&case));
            let got: Vec<(u32, &str, bool, Option<&str>)> = gated.decisions.iter()
                .map(|d| (d.candidate, d.frontier.snap.as_str(), d.good, d.result.as_ref().map(|r| r.snap.as_str()))).collect();
            assert_eq!(got, case.want, "{}", case.name);
            assert_eq!(gated.need_select, case.need_select, "{}", case.name);
            assert_eq!(gated.ranked, case.ranked, "{}", case.name);
        }
        // An invalid score is no state; a later score of the same snapshot never replaces the first.
        let mut r = base(1, None);
        r.extend([score(Tier::Search, "bad", None, None), Record::Eval { n: 1, episode: "r1-w1".into(), candidate: 1, snap: "bad".into(), note: String::new() }]);
        r.extend(state(2, 1, "x2", 2.0, None));
        r.push(score(Tier::Search, "x2", Some(9.0), None));
        let gated = gate(&spec(""), &r);
        assert_eq!((gated.decisions[0].frontier.snap.as_str(), gated.decisions[0].frontier.search), ("x2", 2.0));
    }

    #[test]
    fn audits_happen_when_a_candidate_first_becomes_good_and_at_the_last_gate() {
        let spec = spec("");
        let mut rows = base(3, None);
        for (i, (c, snap, s)) in [(1, "x3", 3.0), (1, "x4", 4.0), (2, "x2", 2.0), (3, "xm1", -1.0)].into_iter().enumerate() {
            rows.extend(state(i + 1, c, snap, s, None));
        }
        let audited = |verdicts: &[(&str, bool)], spec: &Spec, last| -> Vec<(u32, String)> {
            let records: Vec<Record> = rows.iter().cloned().chain(verdicts.iter().map(|(s, p)| verdict(s, *p))).collect();
            audits(spec, &gate(spec, &records), last).iter().map(|d| (d.candidate, d.frontier.snap.clone())).collect()
        };
        // New good candidates are audited, at their frontier; #3, below the seed, is not.
        assert_eq!(audited(&[], &spec, false), vec![(1, "x4".to_string()), (2, "x2".to_string())]);
        // #1's refinement x4 waits while x3 passed; a failed state's candidate is audited at its next state.
        assert_eq!(audited(&[("x3", true), ("x2", false)], &spec, false), vec![]);
        assert_eq!(audited(&[("x4", false), ("x2", true)], &spec, false), vec![(1, "x3".to_string())]);
        // The last gate audits a lift beyond the margin over the passed result.
        assert_eq!(audited(&[("x3", true), ("x2", true)], &spec, true), vec![(1, "x4".to_string())]);
        let strict = Spec { min_delta: 1.5, ..spec.clone() };
        assert!(audited(&[("x3", true), ("x2", true)], &strict, true).is_empty(), "a lift within the margin keeps the passed result unaudited");
    }

    #[test]
    fn the_portfolio_is_built_from_results() {
        let spec = spec("");
        let d = |candidate: u32, frontier: f64, result: Option<(&str, f64)>| Decision { candidate, idea: String::new(),
            frontier: Scored { snap: format!("f{candidate}"), search: frontier, se: None, select: None }, good: true, audit: None,
            result: result.map(|(snap, search)| Scored { snap: snap.into(), search, se: None, select: None }) };
        let picked = portfolio(&spec, &[d(1, 9.0, Some(("a", 3.0))), d(2, 2.0, None), d(3, 5.0, Some(("b", 5.0))),
            d(4, 4.0, Some(("b", 5.0))), d(5, 1.0, Some(("c", 1.0))), d(6, 1.0, Some(("e", 0.5)))]);
        let ids: Vec<u64> = picked.iter().map(|p| p["candidate"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![3, 1, 5], "best result first, one per state, at most three");
    }

    fn plan(label: &str, role: Role) -> Plan {
        Plan { label: label.into(), role, round: 1, candidate: 1, start: "s".into(), best: None, idea: None }
    }

    fn episode(plan: &str, attempt: usize, role: Role, outcome: Outcome) -> Record {
        Record::Episode { plan: plan.into(), attempt, role, outcome, report_ref: None }
    }

    #[test]
    fn interrupted_attempts_never_use_up_an_episode() {
        let mut records = vec![];
        for attempt in 1..=3 {
            for (p, role) in [("r1-w1", Role::Explore), ("r0-research", Role::Research)] {
                records.extend([episode(p, attempt, role, Outcome::Started), episode(p, attempt, role, Outcome::Interrupted)]);
            }
        }
        let left = pending(vec![plan("r1-w1", Role::Explore), plan("r0-research", Role::Research)], &records);
        assert_eq!(left.iter().map(|(p, a, c)| (p.label.as_str(), *a, *c)).collect::<Vec<_>>(), vec![("r1-w1", 4, 0), ("r0-research", 4, 0)]);
        // A worker interrupted after scoring keeps its evals and is done; an auditor that recorded its verdict too.
        let mut records = vec![episode("r1-w2", 1, Role::Explore, Outcome::Started),
            Record::Eval { n: 1, episode: "r1-w2".into(), candidate: 1, snap: "x".into(), note: String::new() },
            episode("r1-w2", 1, Role::Explore, Outcome::Interrupted),
            episode("a1-c1", 1, Role::Audit, Outcome::Started), episode("a1-c1", 1, Role::Audit, Outcome::Interrupted)];
        assert_eq!(pending(vec![plan("a1-c1", Role::Audit)], &records).len(), 1);
        records.push(verdict("s", true));
        assert!(pending(vec![plan("r1-w2", Role::Explore), plan("a1-c1", Role::Audit)], &records).is_empty());
        // An attempt still open counts as an attempt; crashes count; a timed-out attempt is an outcome, not a crash.
        assert_eq!(pending(vec![plan("r1-w5", Role::Explore)], &[episode("r1-w5", 1, Role::Explore, Outcome::Started)])[0].1, 2);
        let records: Vec<Record> = (1..=3).flat_map(|a| [episode("r1-w3", a, Role::Explore, Outcome::Started), episode("r1-w3", a, Role::Explore, Outcome::Crashed)]).collect();
        assert!(pending(vec![plan("r1-w3", Role::Explore)], &records).is_empty());
        let records = vec![episode("r1-w4", 1, Role::Explore, Outcome::Started), episode("r1-w4", 1, Role::Explore, Outcome::TimedOut)];
        assert!(pending(vec![plan("r1-w4", Role::Explore)], &records).is_empty());
    }

    #[test]
    fn a_workers_best_and_improvements_are_derived_from_its_evals() {
        let spec = spec("");
        let mut records = base(1, None);
        for (n, (label, snap, s)) in [("r1-w1", "seed", 0.0), ("r1-w1", "x6", -1.0), ("r1-w1", "x1", -36.0), ("r1-w1.2", "x7", 0.0)].into_iter().enumerate() {
            records.push(score(Tier::Search, snap, Some(s), None));
            records.push(Record::Eval { n: n + 1, episode: label.into(), candidate: 1, snap: snap.into(), note: String::new() });
        }
        let book = Book::of(&records);
        let explore = Plan { start: "seed".into(), ..plan("r1-w1", Role::Explore) };
        // The starting state never improves; the first valid state built sets the bar.
        assert_eq!(progress(&spec, &book, &explore, "r1-w1"), (Some(-1.0), Some("x6".into())));
        // An exploit worker's bar starts at its candidate's frontier, and each attempt counts its own.
        assert_eq!(progress(&spec, &book, &Plan { best: Some(0.5), ..explore.clone() }, "r1-w1"), (Some(0.5), None));
        assert_eq!(progress(&spec, &book, &explore, "r1-w1.2"), (Some(0.0), Some("x7".into())));
    }

    #[test]
    fn progress_needs_a_good_candidate_or_a_real_lift() {
        let decision = |candidate: u32, search: f64, good: bool, se: Option<f64>| Decision { candidate, idea: String::new(),
            frontier: Scored { snap: format!("s{candidate}-{search}"), search, se, select: None }, good, audit: None, result: None };
        let spec = spec("");
        let mut at = BTreeMap::new();
        // A new candidate that is not good is remembered, but its arrival is no progress.
        assert!(!progressed(&spec, &mut at, &[&decision(1, 5.0, false, None)]));
        assert_eq!(at.get(&1), Some(&(5.0, None)));
        // With min_delta 0, a lift below EPSILON is no progress; a real one is.
        assert!(!progressed(&spec, &mut at, &[&decision(1, 5.0 + 1e-12, false, None)]));
        assert!(progressed(&spec, &mut at, &[&decision(1, 5.5, false, None)]));
        // A lift within the evaluator's se is noise.
        assert!(!progressed(&spec, &mut at, &[&decision(1, 5.7, true, Some(0.5))]));
        assert!(progressed(&spec, &mut at, &[&decision(2, 1.0, true, None)]));
        // A candidate that leaves and comes back is not new.
        assert!(!progressed(&spec, &mut at, &[&decision(2, 1.0, true, None)]));
        let strict = Spec { min_delta: 1.0, ..spec };
        assert!(!progressed(&strict, &mut at, &[&decision(2, 1.9, true, None)]));
        assert!(progressed(&strict, &mut at, &[&decision(2, 2.5, true, None)]));
    }

    #[test]
    fn held_out_data_needs_confinement_or_an_acknowledgement() {
        let plain = spec("");
        let held = spec("select = \"python3 s.py\"\n");
        let off: Result<i32, &str> = Err("no landlock");
        assert!(confinement(&held, Ok(3), false).is_ok());
        assert!(confinement(&plain, off, false).is_ok(), "nothing to hide");
        let err = confinement(&held, off, false).unwrap_err().to_string();
        assert!(err.contains("--ar-unconfined") && err.contains("no landlock"), "{err}");
        assert!(confinement(&held, off, true).is_ok());
        assert!(confinement(&spec("grader = [\"/tmp\"]\n"), off, false).is_err());
    }

    #[test]
    fn episodes_spawn_a_copy_of_the_running_binary() {
        let dir = temp("exe");
        let exe = hx_exe(&dir).unwrap();
        assert_eq!(exe, dir.join("hx"));
        assert_eq!(std::fs::read(&exe).unwrap(), std::fs::read(std::env::current_exe().unwrap()).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn templates_fill_every_placeholder() {
        assert_eq!(fill("a {{x}} {{y}} {{z}}\n[[on]] kept\n[[off]] dropped\n", &[("x", "{{y}}".into()), ("y", "2".into())], &["on"]),
            "a {{y}} 2 {{z}}\nkept\n");
        let (dir, o) = orchestrator("tasks", spec("select = \"s\"\n"));
        let mut records = base(1, None);
        records.extend(state(1, 1, "x3", 3.0, None));
        records.push(Record::Verdict { snap: "x3".into(), pass: false, reason: "lookahead".into() });
        let view = View { book: Book::of(&records), brief: "BRIEF".into(), findings: String::new() };
        for role in [Role::Research, Role::Explore, Role::Exploit, Role::Audit] {
            let task = o.task(&Plan { idea: Some("IDEA".into()), best: Some(3.0), ..plan("r1-w1", role) }, "r1-w1", &view).unwrap();
            assert!(!task.contains("{{") && !task.contains("[[") && task.starts_with("PROGRAM") && task.contains("<tools>"), "{role:?}: {task}");
            assert_eq!(task.contains("ar.verdict{"), role == Role::Audit, "{role:?}");
            assert_eq!(task.contains("IDEA"), matches!(role, Role::Explore | Role::Exploit), "{role:?}");
            assert!(task.contains("held-out data no agent sees") && !task.contains("ar.claim"), "{role:?}");
        }
        let exploit = o.task(&plan("r2-w1", Role::Exploit), "r2-w1", &view).unwrap();
        assert!(exploit.contains("failed its audit: lookahead") && exploit.contains("candidate #1") && exploit.contains("idea 1"), "{exploit}");
        for (role, flag) in [(Role::Explore, "explore"), (Role::Exploit, "exploit")] {
            let vars = [o.vars(&plan("r1-w1", role), "r1-w1", &view.book), vec![("seed", "SEED".into()), ("taken", "- T\n".into())]].concat();
            let ask = fill(IDEA, &vars, &[flag, "taken"]);
            assert!(!ask.contains("{{") && !ask.contains("[[") && ask.contains("SEED") && ask.contains("- T") && ask.contains("IDEA: "), "{ask}");
            let dedupe = fill(DEDUPE, &[("taken", "T1. a\n".into()), ("new", "1. b\n".into())], &[flag]);
            assert!(!dedupe.contains("{{") && !dedupe.contains("[[") && dedupe.contains("Duplicates: none"), "{dedupe}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn planned_ideas_and_duplicates_are_read_from_answers() {
        assert_eq!(idea_of("Dimensions: ...\nIDEA: a **breakout** rule\non volume.\n").as_deref(), Some("a **breakout** rule on volume."));
        assert_eq!(idea_of("just this idea").as_deref(), Some("just this idea"));
        assert_eq!(idea_of("IDEA:  "), None);
        assert_eq!(duplicates_of("2 repeats 1.\nDuplicates: 2, 4", 3), vec![2]);
        assert_eq!(duplicates_of("**Duplicates:** none", 3), Vec::<usize>::new());
        assert_eq!(duplicates_of("Answer: 0", 3), Vec::<usize>::new());
        assert_eq!(duplicates_of("no format at all 1 2", 3), Vec::<usize>::new());
    }

    #[test]
    fn episode_labels() {
        for ok in ["r0-research", "r1-w3", "r1-w3.2", "a2-c5", "f4-c1"] { assert!(valid_label(ok), "{ok}"); }
        for bad in ["", "../x", "r1/w1", "r1-w1.", "r1-w1.x", ".2", "r1-w1.2.3"] { assert!(!valid_label(bad), "{bad}"); }
        assert_eq!((attempt_label("r1-w3", 1), attempt_label("r1-w3", 2)), ("r1-w3".to_string(), "r1-w3.2".to_string()));
    }

    #[test]
    fn the_journal_is_one_chained_log_that_replays_and_locks_the_run() {
        let (dir, o) = orchestrator("journal", spec(""));
        let run = o.dir.clone();
        // A run in use is refused; the orchestrator's model calls share the journal and are not run records.
        assert!(open_journal(&run).err().unwrap().to_string().contains("in use by another hx"));
        o.log.write("note", json!({"x": 1}), true).unwrap();
        o.plan("research", vec![plan("r0-research", Role::Research)]).unwrap();
        o.plan("research", vec![plan("r0-other", Role::Research)]).unwrap();
        assert_eq!(o.planned("research").unwrap()[0].label, "r0-research", "a batch is planned once");
        let eval = |n| o.journal_if(|r| r.iter().all(|r| !matches!(r, Record::Eval { snap, .. } if snap == "x"))
            .then(|| Record::Eval { n, episode: "r1-w1".into(), candidate: 1, snap: "x".into(), note: String::new() })).unwrap();
        assert!(eval(1).is_some() && eval(2).is_none());
        let log = o.log.path.clone();
        drop(o);
        std::fs::OpenOptions::new().append(true).open(&log).and_then(|mut f| std::io::Write::write_all(&mut f, b"{\"kind\": \"ga")).unwrap();
        assert_eq!(load(&run).unwrap().len(), 3, "a torn final line is ignored");
        assert!(crate::eventlog::verify(&log).unwrap().chain_ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn children_get_no_held_out_details_and_unresolved_roots_stop_the_run() {
        let dir = temp("child");
        let (grader, holdout) = (dir.join("grader"), dir.join("holdout"));
        std::fs::create_dir_all(&grader).unwrap();
        std::fs::create_dir_all(&holdout).unwrap();
        let (tmp, o) = orchestrator("child-run", spec(&format!("select = \"python3 secret_select.py\"\ngrader = [\"{}\"]\nholdout = [\"{}\"]\n",
            grader.display(), holdout.display())));
        let episode = Episode { label: "r1-w1".into(), role: Role::Explore, editable: o.spec().editable.clone(), sealed: o.sealed().unwrap(), log: dir.join("log") };
        let text = serde_json::to_string(&episode).unwrap();
        assert!(!text.contains("secret_select"), "{text}");
        assert_eq!(sealed(&episode), vec![tmp.join("state/ar"), grader.clone(), holdout.clone(), dir.join("log")]);
        std::fs::remove_dir_all(&holdout).unwrap();
        assert!(o.sealed().is_err(), "an unresolved holdout root must not go unsealed");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn verdicts_are_for_auditors_need_a_reason_to_fail_and_are_final() {
        let (dir, o) = orchestrator("verdict", spec(""));
        let audit = Plan { start: "x3".into(), ..plan("a1-c1", Role::Audit) };
        assert!(o.verdict(&plan("r1-w1", Role::Explore), true, "").unwrap_err().to_string().contains("for auditors"));
        assert!(o.verdict(&audit, false, "  ").unwrap_err().to_string().contains("needs its reason"));
        assert!(o.verdict(&audit, true, "earned\n twice").is_ok());
        assert!(o.verdict(&audit, false, "changed my mind").unwrap_err().to_string().contains("final"));
        assert_eq!(o.book().verdicts.get("x3"), Some(&(true, "earned twice".to_string())));
        let _ = std::fs::remove_dir_all(dir);
    }
}
