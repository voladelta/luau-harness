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
//! A run is research, then rounds of workers side by side, each followed by a gate. Round 1 explores: each worker
//! claims a different idea, and each accepted idea is a candidate. The gate is a pure function of what is journaled
//! (`gate`): for each candidate it derives its frontier, the best state no check rejected, which the next round
//! refines, and its result, the best state that beats the seed by the margin, passes the held-out `select` veto and
//! passed an audit. Exploit rounds refine the strongest frontiers; one that moves none explores again. Two rounds
//! without progress, or `max_rounds`, end the run, and the portfolio is the strongest results. The held-out score is
//! only ever a veto: ranking on it as well would select on it every round until it is as optimistic as the search score.
//!
//! `episode` holds what runs inside an episode (`ar.eval`, `ar.claim`, `ar.claims`, `ar.reset`); `files` holds what
//! crosses its boundary (snapshots, throwaway copies, the evaluator, the frozen base).
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

mod episode;
pub(crate) mod files;
mod ssot;
pub use episode::{Episode, Progress, claim, claims, eval, load_episode, reset};
use files::*;

/// Model turns an episode may take: its whole allowance, in work rather than dollars, so it means the same on any model.
const TURNS: usize = 40;
/// Candidates a gate hands to the next round, and the size of the final portfolio.
const TOP: usize = 3;
/// Ledger rows shown to a worker, and findings (the most recent ones).
const SHOWN: usize = 40;
const FINDINGS: usize = 16;
/// Characters of an evaluator report, of a brief and of one finding shown to an episode.
const REPORT_CHARS: usize = 12_000;
const BRIEF_CHARS: usize = 8_000;
const FINDING_CHARS: usize = 1_500;
/// Crashes of a planned episode (a child that ends with neither a report nor an eval, usually provider errors) before
/// it is given up.
const MAX_CRASHES: usize = 3;
/// The smallest difference that counts as better, relative to the score's magnitude (at least 1): rounding never does.
const EPSILON: f64 = 1e-9;
/// Seconds a child may run past `episode_secs` (finishing its answer, stopping its commands) before the watchdog kills
/// it: one that ignores its own deadline would otherwise hold its round forever.
const GRACE_SECS: f64 = 120.0;
/// The run directory's format. A run started by an hx with another format is refused on resume.
const FORMAT: u32 = 2;

/// Role prompts, kept in `stdlib/std/ar` beside the RPI phase documents and embedded the same way.
const TOOLS: &str = include_str!("../stdlib/std/ar/tools.md");
const RESEARCH: &str = include_str!("../stdlib/std/ar/research.md");
const EXPLORE: &str = include_str!("../stdlib/std/ar/explore.md");
const EXPLOIT: &str = include_str!("../stdlib/std/ar/exploit.md");
const AUDIT: &str = include_str!("../stdlib/std/ar/audit.md");

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

/// What an episode is for. Research writes the brief; explore workers claim and test a new idea; exploit workers claim
/// and test a refinement of one candidate; auditors judge whether a candidate's score is real. Research and audit
/// evaluations are probes: recorded, never a candidate's state.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role { Research, Explore, Exploit, Audit }

/// Paths a child's policy must seal, so that even with --yes its agent cannot read or change them: every
/// autoresearch run of the owner's workspace (this run included) and the evaluators' roots, which the orchestrator
/// resolved before it started the episode (one it cannot resolve stops the run, so nothing is left unsealed).
pub fn sealed(e: &Episode) -> Vec<PathBuf> {
    let mut out = vec![e.dir.parent().unwrap_or(&e.dir).to_path_buf()];
    out.extend(e.sealed.iter().cloned());
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
    /// Whether it beat the worker's best when scored, as the worker was told. The gate derives a candidate's states
    /// from scores alone, never from this.
    improved: bool,
    /// The search evaluator's report, shown to later episodes for a candidate's best state.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    report: Value,
}

/// One accepted claim. Explore claims share the scope `explore` and their number is the candidate's id; an exploit
/// worker's refinement claims are scoped to its candidate.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Claim {
    scope: String,
    n: u32,
    idea: String,
    worker: String,
}

/// The ledger's rows. Read incrementally and kept for the process: the orchestrator and an episode's `ar.eval` read it
/// again and again while it only grows.
fn ledger(dir: &Path) -> Result<Vec<Row>> {
    static CACHE: std::sync::Mutex<Option<Tail<Row>>> = std::sync::Mutex::new(None);
    let path = dir.join("ledger.jsonl");
    let mut cache = CACHE.lock().unwrap();
    if cache.as_ref().is_none_or(|t| t.path != path) { *cache = Some(Tail::new(path)); }
    Ok(cache.as_mut().unwrap().read()?.to_vec())
}

/// Records of an append-only JSONL file. A torn final line (a writer killed mid-append) is ignored; damage anywhere
/// else is an error.
fn read_lines<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let mut tail = Tail::new(path.to_path_buf());
    tail.read()?;
    Ok(tail.rows)
}

/// An append-only JSONL file read incrementally: each read parses only the complete lines added since the last one.
/// (Appends only ever drop a torn line past the last complete one, so what was read stays valid; a file that shrank
/// below it was replaced and is read again.)
struct Tail<T> {
    path: PathBuf,
    len: u64,
    rows: Vec<T>,
}
impl<T: serde::de::DeserializeOwned> Tail<T> {
    fn new(path: PathBuf) -> Tail<T> { Tail { path, len: 0, rows: vec![] } }
    fn read(&mut self) -> Result<&[T]> {
        use std::io::{Read, Seek};
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            (self.len, self.rows) = (0, vec![]);
            return Ok(&self.rows);
        };
        if file.metadata()?.len() < self.len { (self.len, self.rows) = (0, vec![]); }
        file.seek(std::io::SeekFrom::Start(self.len))?;
        let mut text = vec![];
        file.read_to_end(&mut text)?;
        let Some(end) = text.iter().rposition(|b| *b == b'\n') else { return Ok(&self.rows) };
        let mut new = vec![];
        for line in text[..end].split(|b| *b == b'\n').filter(|l| !l.trim_ascii().is_empty()) {
            new.push(serde_json::from_slice(line).with_context(|| format!("{} line {}", self.path.display(), self.rows.len() + new.len() + 1))?);
        }
        self.rows.extend(new);
        self.len += end as u64 + 1;
        Ok(&self.rows)
    }
}

/// Append one record durably, first dropping a torn final line so the file stays parseable. Only the last byte is read
/// unless the line before is torn.
fn append_line<T: Serialize>(path: &Path, record: &T) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    let mut file = std::fs::OpenOptions::new().create(true).read(true).append(true).open(path)?;
    let len = file.metadata()?.len();
    let mut last = [b'\n'];
    if len > 0 { file.read_exact_at(&mut last, len - 1)?; }
    if last[0] != b'\n' {
        let text = std::fs::read(path)?;
        file.set_len(text.iter().rposition(|b| *b == b'\n').map_or(0, |i| i as u64 + 1))?;
    }
    writeln!(file, "{}", serde_json::to_string(record)?)?;
    file.sync_all()?;
    Ok(())
}

/// Launch settings, frozen at the start and reused on resume.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Launch {
    #[serde(default)]
    format: u32,
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
    /// The round it belongs to: 0 for the research, the gate's round for audits.
    round: usize,
    /// The candidate an exploit worker refines or an auditor judges (0 otherwise).
    idea: u32,
    start: String,
    best: Option<f64>,
    /// An explore worker's seed for generating its idea.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seed: Option<String>,
}

/// How an episode attempt ended. Only a crash counts toward `MAX_CRASHES`: an attempt cut short by a stopped
/// orchestrator says nothing about the episode.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    /// The child ended by itself, with a report or with evals.
    Done,
    /// The child ended with neither, usually after provider errors: retried.
    Crashed,
    /// The orchestrator stopped while the child ran; recorded on resume from what the child left.
    Interrupted,
    /// The watchdog killed a child that outlived its wall-clock cap: done, with whatever it scored.
    TimedOut,
}

/// One state of a candidate as a gate sees it: its first valid search evaluation, and its held-out score once computed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Scored {
    snap: String,
    search: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se: Option<f64>,
    #[serde(default)]
    select: Option<f64>,
}

/// A gate's decision for one candidate (an accepted explore claim with a valid state other than the seed).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Decision {
    idea: u32,
    claim: String,
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

/// The seed as a gate compares against it.
#[derive(Clone, Debug, Default)]
struct Seed {
    snap: String,
    search: Option<f64>,
    se: Option<f64>,
    select: Option<f64>,
}

/// The checks journaled so far, by snapshot: held-out scores (None: the select evaluator found it invalid) and audit
/// verdicts with their reasons.
#[derive(Clone, Debug, Default)]
struct Checks {
    selects: HashMap<String, Option<f64>>,
    verdicts: HashMap<String, (bool, String)>,
}

/// What a gate decides: each candidate's decision, the candidates ranked for the next round, and the frontiers whose
/// held-out score it needs before its decisions are final.
#[derive(Debug, Default)]
struct Gated {
    decisions: Vec<Decision>,
    ranked: Vec<u32>,
    need_select: Vec<String>,
}

/// The gate, a pure function of the ledger, the claims and the journaled checks. Per candidate, each state counts once
/// (at its first valid evaluation) and only the seed's own bytes are excluded: a state that merely ties the seed's
/// score is a candidate's state like any other, not good, ranked by search.
///
/// A state is rejected by a failed audit or by the held-out veto (worse than the seed on `select`, or invalid there).
/// The frontier is the best state no check rejected; it is good when it beats the seed by `margin` and passed the veto.
/// The held-out score is looked at only where it can decide something: a frontier that beats the seed and has none yet
/// is listed in `need_select`, and the caller gates again once it is journaled (a veto moves the frontier back, exactly
/// like a failed audit). The result is the best state that beats the seed, passed the veto and passed an audit.
///
/// Candidates rank good ones first, then by search (workers return their ideas unpolished, so one below the seed still
/// earns refinement), then by id; a frontier another candidate ranked higher already holds is left out.
fn gate(spec: &Spec, seed: &Seed, rows: &[Row], claims: &[Claim], checks: &Checks) -> Gated {
    let beats = |s: &Scored| spec.better(s.search, seed.search, margin(spec, s.se, seed.se));
    // Some(true): vetoed; None: not computed.
    let vetoed = |s: &Scored| -> Option<bool> {
        if spec.select.is_none() { return Some(false); }
        let score = checks.selects.get(&s.snap)?;
        Some(score.is_none_or(|x| seed.select.is_some_and(|b| spec.ahead(b, x))))
    };
    let verdict = |s: &Scored| checks.verdicts.get(&s.snap).map(|v| v.0);
    let best = |states: &mut dyn Iterator<Item = &Scored>| -> Option<Scored> {
        states.fold(None::<&Scored>, |b, s| if b.is_none_or(|b| spec.ahead(s.search, b.search)) { Some(s) } else { b })
            .map(|s| Scored { select: checks.selects.get(&s.snap).copied().flatten(), ..s.clone() })
    };
    let mut out = Gated::default();
    for c in claims.iter().filter(|c| c.scope == "explore") {
        let mut states: Vec<Scored> = vec![];
        for r in rows.iter().filter(|r| r.idea == c.n && r.valid && r.snap != seed.snap) {
            let Some(search) = r.score else { continue };
            if !states.iter().any(|s| s.snap == r.snap) { states.push(Scored { snap: r.snap.clone(), search, se: r.se, select: None }); }
        }
        let rejected = |s: &Scored| verdict(s) == Some(false) || vetoed(s) == Some(true);
        let Some(frontier) = best(&mut states.iter().filter(|s| !rejected(s))) else { continue };
        if beats(&frontier) && vetoed(&frontier).is_none() { out.need_select.push(frontier.snap.clone()); }
        let result = best(&mut states.iter().filter(|s| beats(s) && vetoed(s) == Some(false) && verdict(s) == Some(true)));
        out.decisions.push(Decision { idea: c.n, claim: c.idea.clone(), good: beats(&frontier) && vetoed(&frontier) == Some(false),
            audit: verdict(&frontier), frontier, result });
    }
    let mut order: Vec<&Decision> = out.decisions.iter().collect();
    order.sort_by(|a, b| b.good.cmp(&a.good).then_with(|| spec.order(a.frontier.search, b.frontier.search)).then(a.idea.cmp(&b.idea)));
    let mut seen = HashSet::new();
    out.ranked = order.into_iter().filter(|d| seen.insert(d.frontier.snap.clone())).map(|d| d.idea).collect();
    out
}

/// The frontiers a gate audits, among the `TOP` it hands on: good ones without a verdict whose candidate has no result
/// yet (it has just become good, or its earlier state failed). A candidate that already has a result is audited again
/// only at the last gate, and only when its frontier beats that result by the margin: auditing every lift of every
/// candidate every round costs an episode per candidate per round for a result that only the last gate fixes.
fn audits<'a>(spec: &Spec, gated: &'a Gated, last: bool) -> Vec<&'a Decision> {
    gated.ranked.iter().take(TOP).filter_map(|id| gated.decisions.iter().find(|d| d.idea == *id))
        .filter(|d| d.good && d.audit.is_none())
        .filter(|d| d.result.as_ref().is_none_or(|r| last && spec.better(d.frontier.search, Some(r.search), margin(spec, d.frontier.se, r.se))))
        .collect()
}

/// The portfolio: the distinct results of a gate's decisions, best search score first, at most `TOP`.
fn portfolio(spec: &Spec, decisions: &[Decision]) -> Vec<Value> {
    let mut results: Vec<(&Decision, &Scored)> = decisions.iter().filter_map(|d| d.result.as_ref().map(|r| (d, r))).collect();
    results.sort_by(|a, b| spec.order(a.1.search, b.1.search).then(a.0.idea.cmp(&b.0.idea)));
    let mut seen = HashSet::new();
    results.into_iter().filter(|(_, r)| seen.insert(r.snap.clone())).take(TOP)
        .map(|(d, r)| json!({"idea": d.idea, "claim": d.claim, "snap": r.snap, "search": r.search, "se": r.se, "select": r.select}))
        .collect()
}

/// Journal records. A batch is `planned` whole before it starts; an episode attempt is `started` before its child runs
/// and `finished` after (on resume, for one a stopped orchestrator left open). Held-out scores and audit verdicts are
/// journaled once per snapshot and reused; a `gate` records its decisions (`last`: the run's final gate).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Record {
    Planned { batch: String, episodes: Vec<Plan> },
    Started { label: String, plan: String, #[serde(default)] scratch: Option<PathBuf> },
    Finished { label: String, evals: usize, outcome: Outcome },
    Selected { snap: String, score: Option<f64> },
    Audited { idea: u32, snap: String, pass: bool, reason: String },
    Gate { round: usize, #[serde(default)] last: bool, candidates: Vec<Decision>, chosen: Vec<u32> },
}

/// The attempts of a planned episode, in order: (label, outcome once finished, evals).
fn attempts<'a>(records: &'a [Record], plan: &str) -> Vec<(&'a str, Option<Outcome>, usize)> {
    records.iter().filter_map(|r| match r { Record::Started { label, plan: p, .. } if p == plan => Some(label.as_str()), _ => None })
        .map(|label| records.iter().find_map(|r| match r {
            Record::Finished { label: l, evals, outcome } if l == label => Some((label, Some(*outcome), *evals)),
            _ => None,
        }).unwrap_or((label, None, 0)))
        .collect()
}

/// The plans of a batch still to run, each with the label of its next attempt and its crashes so far. A plan is
/// settled by an attempt that finished or timed out, or an interrupted one that left what its role is for (a worker's
/// evals, an auditor's verdict, which `verdict` tells by label); it is given up after `MAX_CRASHES` crashes.
/// Interrupted attempts are no crashes, so stopping the orchestrator any number of times never uses up an episode, and
/// a batch whose attempts were all interrupted is never taken for finished.
fn pending(plans: Vec<Plan>, records: &[Record], verdict: impl Fn(&str) -> bool) -> Vec<(Plan, String, usize)> {
    plans.into_iter().filter_map(|plan| {
        let tries = attempts(records, &plan.label);
        let settled = tries.iter().any(|(label, outcome, evals)| match outcome {
            Some(Outcome::Done | Outcome::TimedOut) => true,
            Some(Outcome::Interrupted) => match plan.role {
                Role::Explore | Role::Exploit => *evals > 0,
                Role::Audit => verdict(label),
                Role::Research => false,
            },
            _ => false,
        });
        let crashes = tries.iter().filter(|t| t.1 == Some(Outcome::Crashed)).count();
        let label = if tries.is_empty() { plan.label.clone() } else { format!("{}.{}", plan.label, tries.len() + 1) };
        (!settled && crashes < MAX_CRASHES).then_some((plan, label, crashes))
    }).collect()
}

/// An auditor's verdict as `ar.verdict` wrote it: `{"pass": bool, "reason": string}`. Anything else is no verdict.
fn read_verdict(path: &Path) -> Option<(bool, String)> {
    let v: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let reason = match &v["reason"] { Value::String(s) => s.chars().take(300).collect(), Value::Null => String::new(), _ => return None };
    Some((v["pass"].as_bool()?, reason))
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
    /// The journal (written only here) and the claims (written by episodes), read incrementally.
    journal: std::sync::Mutex<Tail<Record>>,
    claims: std::sync::Mutex<Tail<Claim>>,
}

impl Orchestrator {
    fn new(workspace: &Path, dir: PathBuf, launch: Launch, exe: PathBuf) -> Orchestrator {
        Orchestrator { workspace: workspace.to_path_buf(), journal: Tail::new(dir.join("journal.jsonl")).into(),
            claims: Tail::new(dir.join("claims.jsonl")).into(), dir, launch, exe }
    }

    fn spec(&self) -> &Spec { &self.launch.spec }

    fn journal(&self, record: &Record) -> Result<()> {
        append_line(&self.dir.join("journal.jsonl"), record)
    }

    fn records(&self) -> Result<Vec<Record>> {
        Ok(self.journal.lock().unwrap().read()?.to_vec())
    }

    fn claims(&self) -> Result<Vec<Claim>> {
        Ok(self.claims.lock().unwrap().read()?.to_vec())
    }

    fn seed_snap(&self) -> Result<String> {
        Ok(std::fs::read_to_string(self.dir.join("seed"))?.trim().to_string())
    }

    /// `seed.json`: the seed's search score, `se` and report, scored by the host before any episode.
    fn seed_json(&self) -> Value {
        std::fs::read(self.dir.join("seed.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
    }

    /// The seed as gates compare against it, with its held-out score when journaled.
    fn seed(&self) -> Result<Seed> {
        let (snap, baseline) = (self.seed_snap()?, self.seed_json());
        let select = self.checks()?.selects.get(&snap).copied().flatten();
        Ok(Seed { search: baseline["score"].as_f64(), se: baseline["se"].as_f64(), select, snap })
    }

    /// Score the seed with the search evaluator before any episode, so every episode knows where it starts. Kept in
    /// `seed.json` (written whole or not at all), outside the ledger, which holds the agents' evaluations.
    async fn baseline(&self) -> Result<()> {
        let (score, valid, se, report) = evaluate(self.spec(), &self.dir, &load_snap(&self.dir, &self.seed_snap()?)?, &self.spec().eval, false).await?;
        eprintln!("hx ar: seed scores {}", if valid { show(score) } else { "invalid".into() });
        crate::atomic_file::replace_trusted(&self.dir.join("seed.json"),
            &serde_json::to_vec(&json!({"score": score.filter(|_| valid), "se": se, "report": report}))?)
    }

    /// The search score and report of `snap`: its first valid evaluation, or the host's baseline for the seed.
    fn known(&self, rows: &[Row], snap: &str) -> (Option<f64>, Value) {
        if let Some(r) = rows.iter().find(|r| r.snap == snap && r.valid) { return (r.score, r.report.clone()); }
        if self.seed_snap().ok().as_deref() != Some(snap) { return (None, Value::Null); }
        let baseline = self.seed_json();
        (baseline["score"].as_f64(), baseline["report"].clone())
    }

    /// The checks journaled so far.
    fn checks(&self) -> Result<Checks> {
        let mut checks = Checks::default();
        for r in self.records()? {
            match r {
                Record::Selected { snap, score } => { checks.selects.insert(snap, score); }
                Record::Audited { snap, pass, reason, .. } => { checks.verdicts.insert(snap, (pass, reason)); }
                _ => {}
            }
        }
        Ok(checks)
    }

    /// The held-out score of `snap` (none without a select evaluator, or when it is invalid), journaled once.
    async fn select(&self, snap: &str) -> Result<Option<f64>> {
        let Some(cmd) = self.spec().select.clone() else { return Ok(None) };
        if let Some(score) = self.checks()?.selects.get(snap) { return Ok(*score); }
        let (score, valid, _, _) = evaluate(self.spec(), &self.dir, &load_snap(&self.dir, snap)?, &cmd, true).await?;
        let score = score.filter(|_| valid);
        self.journal(&Record::Selected { snap: snap.into(), score })?;
        Ok(score)
    }

    /// The gate on everything journaled so far. The held-out scores it needs are computed side by side and journaled,
    /// and it gates again, until it needs none (each pass computes at most one per candidate, and a veto only ever moves
    /// a frontier to a state not yet looked at).
    async fn decide(&self) -> Result<Gated> {
        loop {
            let gated = gate(self.spec(), &self.seed()?, &ledger(&self.dir)?, &self.claims()?, &self.checks()?);
            if gated.need_select.is_empty() { return Ok(gated); }
            futures::future::join_all(gated.need_select.iter().map(|snap| self.select(snap))).await.into_iter().collect::<Result<Vec<_>>>()?;
        }
    }

    /// The final answer of the last finished attempt of a planned episode: a worker's finding, the brief.
    fn answer(&self, records: &[Record], plan: &str) -> Option<String> {
        let (label, ..) = *attempts(records, plan).last()?;
        let report: Value = serde_json::from_slice(&std::fs::read(self.dir.join("episodes").join(format!("{label}.report.json"))).ok()?).ok()?;
        report["final_answer"].as_str().map(str::trim).filter(|a| !a.is_empty()).map(String::from)
    }

    /// The verdict an auditor recorded with `ar.verdict`, from the latest attempt of the plan that wrote one. None when
    /// no attempt did (an auditor that crashed, or stopped at its turn or time cap): its candidate stays unaudited.
    fn verdict(&self, records: &[Record], plan: &str) -> Option<(bool, String)> {
        attempts(records, plan).iter().rev().find_map(|(label, ..)| read_verdict(&self.verdict_path(label)))
    }

    fn verdict_path(&self, label: &str) -> PathBuf {
        self.dir.join("episodes").join(format!("{label}.verdict.json"))
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

    /// Run a planned batch side by side until every episode is settled or has crashed `MAX_CRASHES` times.
    async fn run_batch(&mut self, batch: &str) -> Result<()> {
        let mut crashes = 0;
        loop {
            let records = self.records()?;
            let plans = self.planned(batch)?.unwrap_or_default();
            let pending = pending(plans, &records, |label| read_verdict(&self.verdict_path(label)).is_some());
            if pending.is_empty() { return Ok(()); }
            let retries_left = pending.iter().any(|(.., crashed)| crashed + 1 < MAX_CRASHES);
            let view = self.view()?;
            let mut runs = vec![];
            for (plan, label, _) in &pending {
                runs.push(self.episode(plan, label, self.task(plan, label, &view)?));
            }
            let ran = futures::future::join_all(runs).await.into_iter().collect::<Result<Vec<_>>>()?;
            if !ran.iter().all(|o| o.crashed) { crashes = 0; continue; }
            crashes += 1;
            // Before the first eval of the whole run, repeated crashes mean misconfiguration: stop, don't wait hours.
            if ledger(&self.dir)?.is_empty() && crashes >= MAX_CRASHES {
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

    /// One fresh child run in its own copy of the base holding its starting state.
    async fn episode(&self, plan: &Plan, label: &str, task: String) -> Result<Ran> {
        use tokio::io::AsyncReadExt;
        let ep_dir = self.dir.join("episodes");
        std::fs::create_dir_all(&ep_dir)?;
        ensure!(valid_label(label), "invalid episode label {label:?} in the journal");
        let ep_path = ep_dir.join(format!("{label}.json"));
        crate::atomic_file::replace_trusted(&ep_path, &serde_json::to_vec_pretty(&self.child_episode(plan, label)?)?)?;
        let task_path = ep_dir.join(format!("{label}.md"));
        std::fs::write(&task_path, task)?;
        let scratch = Scratch::new("ws")?;
        let ws = scratch.0.join("ws");
        copy_tree(&self.dir.join("base"), &ws)?;
        place(&ws, self.spec(), &load_snap(&self.dir, &plan.start)?)?;
        // The child's temp directory, its policy's private one included, is inside its copy: whatever a killed child
        // leaves there is deleted with the copy.
        let tmp = scratch.0.join("tmp");
        std::fs::create_dir(&tmp)?;
        self.journal(&Record::Started { label: label.into(), plan: plan.label.clone(), scratch: Some(scratch.0.clone()) })?;
        eprintln!("hx ar: {label} ({:?}{})", plan.role, if plan.idea > 0 { format!(" #{}", plan.idea) } else { String::new() });
        let mut child = tokio::process::Command::new(&self.exe)
            .arg("run").arg("--workspace").arg(&ws).args(&self.launch.child_args)
            .arg("--prompt-file").arg(&task_path)
            .arg("--max-turns").arg(TURNS.to_string())
            .arg("--max-wall").arg(self.spec().episode_secs.to_string())
            .env("HX_AR_EPISODE", &ep_path).env("TMPDIR", &tmp)
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .spawn()?;
        // Its stdin stays open while this orchestrator lives; if it stops, the child stops with it.
        let (_alive, _tracked) = (child.stdin.take(), child.id().map(crate::prims::Child::track));
        let drain = |pipe: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>| tokio::spawn(async move {
            let mut buf = vec![];
            if let Some(mut pipe) = pipe { let _ = pipe.read_to_end(&mut buf).await; }
            buf
        });
        let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
        let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
        let timed_out = match tokio::time::timeout(self.watchdog(), child.wait()).await {
            Ok(status) => { status?; false }
            Err(_) => {
                eprintln!("hx ar: {label} outlived its {}s cap; killing it", self.spec().episode_secs);
                let _ = child.start_kill();
                let _ = child.wait().await;
                true
            }
        };
        // A process the child left holding its pipes must not hold the round either.
        let collect = |h: tokio::task::JoinHandle<Vec<u8>>| async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), h).await.ok().and_then(Result::ok).unwrap_or_default()
        };
        let (stdout, stderr) = (collect(stdout).await, collect(stderr).await);
        std::fs::write(ep_dir.join(format!("{label}.stderr")), &stderr)?;
        let report = String::from_utf8_lossy(&stdout).lines().rev()
            .find_map(|l| serde_json::from_str::<Value>(l).ok()).unwrap_or(Value::Null);
        std::fs::write(ep_dir.join(format!("{label}.report.json")), serde_json::to_vec_pretty(&report)?)?;
        drop(scratch);
        let evals = ledger(&self.dir)?.iter().filter(|r| r.episode == label).count();
        let outcome = if timed_out { Outcome::TimedOut } else if evals == 0 && report.is_null() { Outcome::Crashed } else { Outcome::Done };
        self.journal(&Record::Finished { label: label.into(), evals, outcome })?;
        let end = if timed_out { "timed out" } else if report["finished"] == true { "finished" } else { report["stopped"].as_str().unwrap_or("stopped") };
        eprintln!("hx ar: {label}: {evals} evals, {end}");
        Ok(Ran { crashed: outcome == Outcome::Crashed, stderr: String::from_utf8_lossy(&stderr).into_owned() })
    }

    /// The episode a child serves, with only what it needs: it records the episode in its frozen policy, which its
    /// own shell can read, so the held-out evaluator and roots are left out (its `sealed` still names the roots, so
    /// its policy can seal them; a root that does not resolve stops the run rather than going unsealed).
    fn child_episode(&self, plan: &Plan, label: &str) -> Result<Episode> {
        let spec = Spec { select: None, holdout: vec![], ..self.spec().clone() };
        Ok(Episode { dir: self.dir.clone(), label: label.into(), spec, sealed: self.spec().roots(true)?, role: plan.role,
            idea: plan.idea, start: plan.start.clone(), best: plan.best })
    }

    /// What the journal, ledger and claims say, as episodes are shown it.
    fn view(&self) -> Result<View> {
        let records = self.records()?;
        let rows = ledger(&self.dir)?;
        let claims = self.claims()?;
        let plans: HashMap<&str, &Plan> = records.iter().filter_map(|r| match r { Record::Planned { episodes, .. } => Some(episodes), _ => None })
            .flatten().map(|p| (p.label.as_str(), p)).collect();
        // Every attempt's plan, by attempt label.
        let of: HashMap<String, Plan> = records.iter().filter_map(|r| match r {
            Record::Started { label, plan, .. } => plans.get(plan.as_str()).map(|p| (label.clone(), (*p).clone())),
            _ => None,
        }).collect();
        let brief = self.answer(&records, "r0-research").unwrap_or_else(|| "(none: the research episode wrote no brief)".into());
        // A finding per worker attempt that did not crash: its claim, best score and final answer; the most recent are shown.
        let mut findings = vec![];
        for r in &records {
            let Record::Finished { label, outcome, .. } = r else { continue };
            let Some(plan) = of.get(label).filter(|p| matches!(p.role, Role::Explore | Role::Exploit) && *outcome != Outcome::Crashed) else { continue };
            let claim = claims.iter().rev().find(|c| c.worker == *label).map_or("(no accepted claim)".to_string(), |c| match plan.role {
                Role::Explore => format!("idea #{}: {}", c.n, c.idea),
                _ => format!("refinement of #{}: {}", plan.idea, c.idea),
            });
            let best = rows.iter().filter(|x| x.episode == *label && x.valid).filter_map(|x| x.score)
                .fold(None, |b: Option<f64>, s| if b.is_none_or(|b| self.spec().ahead(s, b)) { Some(s) } else { b });
            let text = std::fs::read(self.dir.join("episodes").join(format!("{label}.report.json"))).ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v["final_answer"].as_str().map(String::from)).unwrap_or_default();
            findings.push(format!("### {label}: {} (best {})\n\n{}\n\n", claim.replace('\n', " "), show(best),
                clip(text.trim(), FINDING_CHARS)));
        }
        let skip = findings.len().saturating_sub(FINDINGS);
        let findings = if skip > 0 { format!("({skip} earlier findings omitted)\n\n") } else { String::new() } + &findings[skip..].concat();
        Ok(View { checks: self.checks()?, seed: self.seed()?, rows, claims, brief, findings, plans: of })
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

    fn task(&self, plan: &Plan, label: &str, view: &View) -> Result<String> {
        let spec = self.spec();
        let (score, report) = self.known(&view.rows, &plan.start);
        let report = clip(&serde_json::to_string_pretty(&report).unwrap_or_default(), REPORT_CHARS);
        let mut context = format!("{}\n\n", self.launch.program.trim_end());
        if plan.role != Role::Research {
            context += &format!("<brief>\n{}\n</brief>\n\n", clip(&view.brief, BRIEF_CHARS));
            if !view.findings.is_empty() { context += &format!("<findings>\n{}</findings>\n\n", view.findings); }
        }
        context += &format!("<starting_state>\nYour workspace holds it (search score {}). Its evaluator report:\n```json\n{report}\n```\n</starting_state>\n\n", show(score));
        let claim = view.claims.iter().find(|c| c.scope == "explore" && c.n == plan.idea).map_or(String::new(), |c| c.idea.clone());
        let vars = |extra: Vec<(&'static str, String)>| -> Vec<(&'static str, String)> {
            let mut vars = vec![("label", label.to_string()), ("workers", spec.workers.to_string()),
                ("editable", spec.editable.join(", ")), ("idea", plan.idea.to_string()), ("claim", claim.clone())];
            vars.extend(extra);
            vars
        };
        let tools = self.tools(plan.role);
        Ok(match plan.role {
            Role::Research => format!("{context}{tools}{}", fill(RESEARCH, &vars(vec![]), &[])),
            Role::Explore => {
                // Only earlier rounds' claims: a retried worker gets its own claim back from ar.claim and must keep building it.
                let taken: String = view.claims.iter()
                    .filter(|c| c.scope == "explore" && view.plans.get(&c.worker).is_some_and(|p| p.round != plan.round))
                    .map(|c| format!("- #{}: {}\n", c.n, c.idea)).collect();
                let taken = if taken.is_empty() { String::new() } else {
                    format!(" Earlier rounds already claimed these ideas; yours must use a different core approach:\n{}", clip(&taken, BRIEF_CHARS))
                };
                let seed = plan.seed.clone().unwrap_or_else(|| label.to_string());
                format!("{context}{tools}{}", fill(EXPLORE, &vars(vec![("seed", seed), ("taken", taken)]), &[]))
            }
            Role::Exploit => {
                let rows: Vec<&Row> = view.rows.iter().filter(|r| r.idea == plan.idea).collect();
                let skip = rows.len().saturating_sub(SHOWN);
                let mut ledger = format!("<ledger>\nn | score | worker | note\n{}", if skip > 0 { format!("({skip} older rows omitted)\n") } else { String::new() });
                for r in &rows[skip..] {
                    let score = match (r.valid, r.score) { (true, Some(s)) => format!("{s:.6}"), _ => "invalid".into() };
                    ledger += &format!("{} | {score} | {} | {}\n", r.n, r.episode, r.note.replace('\n', " "));
                }
                format!("{context}{ledger}</ledger>\n\n{}{tools}{}", rejections(spec, view, &rows),
                    fill(EXPLOIT, &vars(vec![("best", show(plan.best))]), &[]))
            }
            Role::Audit => format!("{context}{tools}{}", fill(AUDIT, &vars(vec![]), &[])),
        })
    }

    /// The gate after `round`, or the run's last gate: audit what `audits` picks, in one batch, then journal the
    /// decisions and the `TOP` candidates the next round refines.
    async fn gate(&mut self, round: usize, last: bool) -> Result<Vec<u32>> {
        let batch = if last { "audit-last".to_string() } else { format!("audit{round}") };
        // A resumed run finishes the batch it planned, so verdicts come from that batch's own plans.
        let plans = match self.planned(&batch)? {
            Some(plans) => plans,
            None => {
                let gated = self.decide().await?;
                let plans: Vec<Plan> = audits(self.spec(), &gated, last).into_iter()
                    .map(|d| Plan { label: format!("{}{round}-c{}", if last { "f" } else { "a" }, d.idea), role: Role::Audit, round,
                        idea: d.idea, start: d.frontier.snap.clone(), best: None, seed: None }).collect();
                if !plans.is_empty() { self.plan(&batch, plans.clone())?; }
                plans
            }
        };
        if !plans.is_empty() {
            self.run_batch(&batch).await?;
            self.judge_audits(&plans)?;
        }
        let gated = self.decide().await?;
        let chosen: Vec<u32> = gated.ranked.iter().take(TOP).copied().collect();
        eprintln!("hx ar: {} {round}: {} candidates, {} good, {} with a result, going on with {:?}", if last { "last gate after round" } else { "gate" },
            gated.decisions.len(), gated.decisions.iter().filter(|c| c.good).count(), gated.decisions.iter().filter(|c| c.result.is_some()).count(), chosen);
        self.journal(&Record::Gate { round, last, candidates: gated.decisions, chosen: chosen.clone() })?;
        Ok(chosen)
    }

    /// Journal the verdict of each audit plan whose state has none yet.
    fn judge_audits(&self, plans: &[Plan]) -> Result<()> {
        let (records, checks) = (self.records()?, self.checks()?);
        for plan in plans.iter().filter(|p| !checks.verdicts.contains_key(&p.start)) {
            let Some((pass, reason)) = self.verdict(&records, &plan.label) else {
                eprintln!("hx ar: audit of #{}: no verdict; it stays unaudited", plan.idea);
                continue;
            };
            eprintln!("hx ar: audit of #{}: {}{}", plan.idea, if pass { "pass" } else { "FAIL" }, if reason.is_empty() { String::new() } else { format!(" ({reason})") });
            self.journal(&Record::Audited { idea: plan.idea, snap: plan.start.clone(), pass, reason })?;
        }
        Ok(())
    }

    /// Close the attempts a stopped orchestrator left open, durably, with the evals they made, and delete their
    /// scratch copies (the child's temp directory with them).
    fn close(&self) -> Result<()> {
        let (records, rows) = (self.records()?, ledger(&self.dir)?);
        for r in &records {
            let Record::Started { label, scratch, .. } = r else { continue };
            if records.iter().any(|f| matches!(f, Record::Finished { label: l, .. } if l == label)) { continue; }
            // The journal names the directory, but only one shaped like an episode's scratch copy is cleaned up: a
            // recorded path is not authority to kill processes and delete files anywhere else.
            let scratch = scratch.clone().filter(|s| {
                let ok = valid_scratch(s);
                if !ok { eprintln!("hx ar: ignoring invalid recorded scratch directory {}", s.display()); }
                ok && s.is_dir()
            });
            if let Some(dir) = scratch { drop(Scratch(dir)); }
            let evals = rows.iter().filter(|r| r.episode == *label).count();
            self.journal(&Record::Finished { label: label.clone(), evals, outcome: Outcome::Interrupted })?;
        }
        Ok(())
    }

    async fn run(&mut self) -> Result<Value> {
        self.close()?;
        if !self.dir.join("seed.json").exists() { self.baseline().await?; }
        // A seed the search evaluator cannot score leaves nothing to beat; a select evaluator that cannot score it would
        // veto every candidate. Either is a broken setup: stop before spending.
        ensure!(self.seed()?.search.is_some(), "the search evaluator gives no valid score for the seed; fix it before searching. Its report: {}",
            clip(&self.seed_json()["report"].to_string(), 2_000));
        let seed = self.seed_snap()?;
        if self.spec().select.is_some() {
            ensure!(self.select(&seed).await?.is_some(), "the select evaluator gives no valid score for the seed; fix it before searching");
        }
        self.plan("research", vec![Plan { label: "r0-research".into(), role: Role::Research, round: 0, idea: 0, start: seed.clone(), best: None, seed: None }])?;
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
            let mut gate = self.records()?.into_iter().find_map(|r| match r {
                Record::Gate { round: g, last: false, candidates, chosen } if g == round => Some((candidates, chosen)),
                _ => None,
            });
            let plans = match self.planned(&batch)? {
                Some(plans) => plans,
                None if gate.is_some() => vec![],
                None => {
                    let workers = self.spec().workers;
                    let plans = if explore {
                        (1..=workers).map(|w| -> Result<Plan> {
                            Ok(Plan { label: format!("r{round}-w{w}"), role: Role::Explore, round, idea: 0, start: seed.clone(), best: None,
                                seed: Some(ssot::seed()?) })
                        }).collect::<Result<Vec<_>>>()?
                    } else {
                        let (candidates, chosen) = self.last_gate()?.unwrap_or_default();
                        (0..workers).map(|w| {
                            let c = candidates.iter().find(|c| c.idea == chosen[w % chosen.len()]).expect("chosen candidates are listed");
                            Plan { label: format!("r{round}-w{}", w + 1), role: Role::Exploit, round, idea: c.idea, start: c.frontier.snap.clone(),
                                best: Some(c.frontier.search), seed: None }
                        }).collect()
                    };
                    self.plan(&batch, plans.clone())?;
                    plans
                }
            };
            if gate.is_none() {
                self.run_batch(&batch).await?;
                self.gate(round, false).await?;
                gate = self.last_gate()?;
            }
            rounds = round;
            let (candidates, chosen) = gate.unwrap_or_default();
            // With no candidate after a gate the run stops (in practice after round 1: later gates keep earlier candidates).
            if chosen.is_empty() { break; }
            let chosen: Vec<&Decision> = chosen.iter().filter_map(|id| candidates.iter().find(|c| c.idea == *id)).collect();
            let progress = progressed(self.spec(), &mut at, &chosen);
            let moved = chosen.iter().any(|c| !prev.contains(&(c.idea, c.frontier.snap.clone())));
            if !progress && quiet {
                eprintln!("hx ar: two rounds in a row made no progress; stopping");
                break;
            }
            if round == self.spec().max_rounds { eprintln!("hx ar: {round} rounds, the most this run takes; stopping"); }
            quiet = !progress;
            explore = !moved && plans.first().is_some_and(|p| p.role == Role::Exploit);
            prev = chosen.iter().map(|c| (c.idea, c.frontier.snap.clone())).collect();
        }
        ensure!(!ledger(&self.dir)?.is_empty(), "autoresearch made no evaluations; see {}", self.dir.join("episodes").display());
        if !self.records()?.iter().any(|r| matches!(r, Record::Gate { last: true, .. })) { self.gate(rounds, true).await?; }
        self.finish()
    }

    /// Write the result from the last journaled gate, starting nothing: the portfolio's states into `portfolio/c<id>/`
    /// and the best into the workspace's editable paths. When no candidate has a result the workspace is not touched.
    fn finish(&self) -> Result<Value> {
        let rows = ledger(&self.dir)?;
        ensure!(!rows.is_empty(), "autoresearch made no evaluations; see {}", self.dir.join("episodes").display());
        let records = self.records()?;
        let (candidates, chosen) = self.last_gate()?.context("this autoresearch run has no gate yet: nothing to finish")?;
        let portfolio = portfolio(self.spec(), &candidates);
        // Each written whole or not at all: placed in a temporary directory and renamed, so an existing one is complete.
        for p in &portfolio {
            let dir = self.dir.join("portfolio").join(format!("c{}", p["idea"]));
            if dir.exists() { continue; }
            let tmp = self.dir.join("portfolio").join(format!(".c{}.tmp", p["idea"]));
            let _ = std::fs::remove_dir_all(&tmp);
            place(&tmp, self.spec(), &load_snap(&self.dir, p["snap"].as_str().unwrap())?)?;
            std::fs::rename(&tmp, &dir)?;
        }
        let seed = self.seed()?;
        let (snap, search, select) = match portfolio.first() {
            Some(p) => {
                place(&self.workspace, self.spec(), &load_snap(&self.dir, p["snap"].as_str().unwrap())?)?;
                (p["snap"].as_str().unwrap().to_string(), p["search"].as_f64(), p["select"].as_f64())
            }
            None => {
                eprintln!("hx ar: no candidate beat the seed; the workspace is unchanged");
                (seed.snap, seed.search, seed.select)
            }
        };
        let report = json!({
            "run": self.dir.file_name().unwrap().to_string_lossy(), "dir": self.dir, "commit": self.launch.commit,
            "snap": snap, "search": search, "select": select, "portfolio": portfolio, "candidates": candidates, "chosen": chosen,
            "rounds": records.iter().filter(|r| matches!(r, Record::Gate { last: false, .. })).count(),
            "evals": rows.len(),
        });
        crate::atomic_file::replace_trusted(&self.dir.join("report.json"), &serde_json::to_vec_pretty(&report)?)?;
        Ok(report)
    }

    /// The last gate's decisions and the candidates it chose.
    fn last_gate(&self) -> Result<Option<(Vec<Decision>, Vec<u32>)>> {
        Ok(self.records()?.into_iter().rev().find_map(|r| match r { Record::Gate { candidates, chosen, .. } => Some((candidates, chosen)), _ => None }))
    }
}

/// The checks that rejected states of an exploit worker's candidate, with their reasons, so its next workers do not
/// bring back a hole an auditor found or a change the held-out data refused. The held-out score itself is never shown.
fn rejections(spec: &Spec, view: &View, rows: &[&Row]) -> String {
    let mut out = String::new();
    let mut seen = HashSet::new();
    for r in rows.iter().filter(|r| r.valid && seen.insert(r.snap.clone())) {
        let score = show(r.score);
        if let Some((false, reason)) = view.checks.verdicts.get(&r.snap) {
            out += &format!("- State n={} (score {score}) failed its audit: {}\n", r.n, if reason.is_empty() { "no reason given" } else { reason });
        }
        let vetoed = spec.select.is_some() && view.checks.selects.get(&r.snap)
            .is_some_and(|s| s.is_none_or(|x| view.seed.select.is_some_and(|b| spec.ahead(b, x))));
        if vetoed {
            out += &format!("- State n={} (score {score}) was vetoed: it did worse than the starting point on held-out data no agent sees, so it does not generalize.\n", r.n);
        }
    }
    if out.is_empty() { out } else { format!("<checks>\nThese states of this candidate were rejected; the candidate fell back to its best state no check rejected:\n{out}</checks>\n\n") }
}

/// What the journal, ledger and claims say, as episodes are shown it.
struct View {
    rows: Vec<Row>,
    claims: Vec<Claim>,
    brief: String,
    findings: String,
    checks: Checks,
    seed: Seed,
    /// Each attempt's plan, by attempt label.
    plans: HashMap<String, Plan>,
}

/// Did this gate make progress? A chosen candidate makes it when it is good and new, or when its frontier beats its
/// score at the last progress by the margin. A candidate that is not good is remembered when first chosen, so its later
/// lifts count but its arrival does not.
fn progressed(spec: &Spec, at: &mut BTreeMap<u32, (f64, Option<f64>)>, chosen: &[&Decision]) -> bool {
    let progress = chosen.iter().any(|c| match at.get(&c.idea) {
        None => c.good,
        Some(&(before, se)) => spec.better(c.frontier.search, Some(before), margin(spec, c.frontier.se, se)),
    });
    for c in chosen {
        if progress || !at.contains_key(&c.idea) { at.insert(c.idea, (c.frontier.search, c.frontier.se)); }
    }
    progress
}

/// An episode label as the orchestrator makes them (`r1-w3`, `a2-c5`, a retry `r1-w3.2`): it names files in the run.
fn valid_label(label: &str) -> bool {
    let (name, retry) = label.split_once('.').unwrap_or((label, "1"));
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !retry.is_empty() && retry.bytes().all(|b| b.is_ascii_digit())
}

/// A role prompt from its template: a `[[flag]] ` line is kept only when `flag` is set, and each `{{name}}` is replaced
/// by its value in one pass, so a value that contains braces (a claim, a report) is never expanded again.
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

/// Without kernel confinement an episode's shell can read the grader and held-out roots and write the run directory, so
/// a spec that has any of them refuses to run unless the owner acknowledges it. A spec with none has nothing to hide
/// beyond what the agent may see; it runs with the usual note.
fn confinement(spec: &Spec, status: Result<i32, &str>, acknowledged: bool) -> Result<()> {
    let Err(why) = status else { return Ok(()) };
    if spec.select.is_none() && spec.grader.is_empty() && spec.holdout.is_empty() { return Ok(()); }
    ensure!(acknowledged, "--ar cannot confine episodes and evaluators here ({why}): their shell commands could read the \
grader and held-out data and rewrite the run's records. Run hx in a sandbox of your own and pass --ar-unconfined to go ahead");
    eprintln!("hx ar: running unconfined ({why}), as --ar-unconfined allows: the grader and held-out data are only as safe as your own sandbox");
    Ok(())
}

/// The hx binary episodes run, resolved once at launch. On Linux it is `/proc/self/exe`, which names this process's
/// own executable even after the file at its path is replaced or deleted, so rebuilding hx during a run can neither
/// change nor break later episodes (a copy into the run directory would do the same at the cost of a binary per run).
/// Elsewhere it is the path this process started from.
fn hx_exe() -> Result<PathBuf> {
    let proc = Path::new("/proc/self/exe");
    if cfg!(target_os = "linux") && proc.exists() { return Ok(proc.to_path_buf()); }
    Ok(std::env::current_exe()?)
}

/// `hx run --ar`: search until two rounds in a row make no progress, or continue the run in `resume` (a run directory).
pub async fn orchestrate(workspace: &Path, start: Option<(Vec<String>, String)>, resume: Option<PathBuf>, options: Options) -> Result<()> {
    // A resumed run uses the spec frozen in its sealed launch.json, approved when it started; only a launch reads ar.toml.
    // The run directory is in the state root, which no agent can reach (file tools are sealed from it, shell commands
    // neither see nor write it), as safe as the trust store, so launch.json needs no second record there. A directory
    // elsewhere (a run from before the state root, in `.harness/ar`) is refused: shell commands could have rewritten it.
    // An orchestrator holds its run's lock while it runs.
    let (dir, launch, resumed, _lock) = match (start, resume) {
        (_, Some(dir)) => {
            let dir = dir.canonicalize().with_context(|| format!("autoresearch run {}", dir.display()))?;
            let runs = crate::state::ar_dir(workspace)?;
            ensure!(dir.parent() == Some(runs.as_path()), "--resume for --ar takes one of this workspace's runs in {} (see `hx runs`); \
a run kept in the workspace's .harness/ar predates that and cannot be resumed", runs.display());
            let lock = lock_run(&dir)?;
            let launch: Value = serde_json::from_slice(&std::fs::read(dir.join("launch.json"))?)?;
            ensure!(launch["format"].as_u64() == Some(FORMAT as u64), "autoresearch run {} was started by an older hx, whose records \
this version reads differently; it cannot be resumed or finished here: start a new run", dir.display());
            let launch: Launch = serde_json::from_value(launch)?;
            launch.spec.check(workspace)?;
            ensure!(!dir.join("report.json").exists(), "autoresearch run {} already finished", dir.display());
            if !options.finish { confinement(&launch.spec, crate::confine::status(), options.unconfined)?; }
            (dir, launch, true, lock)
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
            let lock = lock_run(&dir)?;
            let commit = freeze(workspace, &dir)?;
            crate::atomic_file::replace_trusted(&dir.join("seed"), save_snap(&dir, &capture(workspace, &spec)?)?.as_bytes())?;
            let launch = Launch { format: FORMAT, spec, program, child_args, commit };
            crate::atomic_file::replace_trusted(&dir.join("launch.json"), &serde_json::to_vec_pretty(&launch)?)?;
            (dir, launch, false, lock)
        }
        (None, None) => bail!("--ar needs a program, or --resume RUN_DIR"),
    };
    let mut o = Orchestrator::new(workspace, dir, launch, hx_exe()?);
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

/// The run's lock, held while an orchestrator works on it: a second `--resume` would take the live run's episodes for
/// crashed ones, kill them and delete their copies.
fn lock_run(dir: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("run.lock"))?;
    ensure!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "autoresearch run {} is in use by another hx; wait for it to stop", dir.display());
    Ok(file)
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

    #[test]
    fn audit_verdicts_come_from_the_verdict_file_and_fail_closed() {
        let dir = temp("verdict");
        let path = dir.join("a1-c1.verdict.json");
        let read = |text: &str| { std::fs::write(&path, text).unwrap(); read_verdict(&path) };
        assert_eq!(read(r#"{"pass": true, "reason": "earned"}"#), Some((true, "earned".into())));
        assert_eq!(read(r#"{"pass": false, "reason": "lookahead"}"#), Some((false, "lookahead".into())));
        assert_eq!(read(r#"{"pass": true}"#), Some((true, String::new())));
        // Anything but a boolean pass is no verdict: the candidate stays unaudited.
        for bad in [r#"{"pass": "true"}"#, r#"{"reason": "x"}"#, r#"{"pass": true, "reason": 3}"#, "VERDICT: PASS", ""] {
            assert_eq!(read(bad), None, "{bad}");
        }
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_verdict(&path), None);
        let _ = std::fs::remove_dir_all(dir);
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

    fn row(n: usize, idea: u32, snap: &str, score: f64) -> Row {
        Row { n, episode: format!("r1-w{idea}"), idea, note: String::new(), snap: snap.into(), score: Some(score), valid: true,
            se: None, improved: false, report: Value::Null }
    }

    fn claim(n: u32) -> Claim {
        Claim { scope: "explore".into(), n, idea: format!("idea {n}"), worker: format!("r1-w{n}") }
    }

    /// The table: the seed scores 0 on search and 0 on select; select scores are given per state.
    struct Case {
        name: &'static str,
        spec: &'static str,
        seed_se: Option<f64>,
        rows: Vec<Row>,
        selects: Vec<(&'static str, Option<f64>)>,
        verdicts: Vec<(&'static str, bool)>,
        /// Per candidate: (frontier snap, good, result snap).
        want: Vec<(u32, &'static str, bool, Option<&'static str>)>,
        need_select: Vec<&'static str>,
        ranked: Vec<u32>,
    }

    #[test]
    fn the_gate_derives_frontiers_and_results() {
        let select = "select = \"s\"\n";
        let cases = vec![
            Case { name: "a select veto falls back like a failed audit (finding 1)", spec: select, seed_se: None,
                rows: vec![row(1, 1, "x3", 3.0), row(2, 1, "x11", 11.0)], selects: vec![("x3", Some(1.0)), ("x11", Some(-5.0))],
                verdicts: vec![("x3", true)], want: vec![(1, "x3", true, Some("x3"))], need_select: vec![], ranked: vec![1] },
            Case { name: "an unaudited refinement keeps the passed result (finding 1)", spec: select, seed_se: None,
                rows: vec![row(1, 1, "x3", 3.0), row(2, 1, "x4", 4.0)], selects: vec![("x3", Some(1.0)), ("x4", Some(1.0))],
                verdicts: vec![("x3", true)], want: vec![(1, "x4", true, Some("x3"))], need_select: vec![], ranked: vec![1] },
            Case { name: "a frontier that beats the seed needs its held-out score first", spec: select, seed_se: None,
                rows: vec![row(1, 1, "x3", 3.0)], selects: vec![], verdicts: vec![],
                want: vec![(1, "x3", false, None)], need_select: vec!["x3"], ranked: vec![1] },
            Case { name: "the fallback is the best surviving state, improved or not (finding 8)", spec: "", seed_se: None,
                rows: vec![row(1, 1, "x3", 3.0), row(2, 1, "x6", 6.0), row(3, 1, "x5", 5.0)], selects: vec![],
                verdicts: vec![("x6", false), ("x5", true)], want: vec![(1, "x5", true, Some("x5"))], need_select: vec![], ranked: vec![1] },
            Case { name: "only the seed's bytes are excluded; a tie is not good and ranks by search (finding 9)", spec: "", seed_se: None,
                rows: vec![row(1, 1, "seed", 0.0), row(2, 1, "x5", 0.0), row(3, 2, "xm5", -1.0)], selects: vec![], verdicts: vec![],
                want: vec![(1, "x5", false, None), (2, "xm5", false, None)], need_select: vec![], ranked: vec![1, 2] },
            Case { name: "a lift within the evaluator's noise is not good (finding 10)", spec: "", seed_se: Some(0.5),
                rows: vec![Row { se: Some(0.5), ..row(1, 1, "x01", 0.1) }, Row { se: Some(0.1), ..row(2, 2, "x2", 2.0) }],
                selects: vec![], verdicts: vec![("x01", true), ("x2", true)],
                want: vec![(1, "x01", false, None), (2, "x2", true, Some("x2"))], need_select: vec![], ranked: vec![2, 1] },
            Case { name: "min_delta is the bar against the seed", spec: "min_delta = 2\n", seed_se: None,
                rows: vec![row(1, 1, "x1", 1.0), row(2, 2, "x3", 3.0)], selects: vec![], verdicts: vec![],
                want: vec![(1, "x1", false, None), (2, "x3", true, None)], need_select: vec![], ranked: vec![2, 1] },
            Case { name: "a candidate whose every state was rejected drops out; invalid select vetoes", spec: select, seed_se: None,
                rows: vec![row(1, 1, "x3", 3.0), row(2, 2, "x4", 4.0)], selects: vec![("x3", None), ("x4", Some(0.0))],
                verdicts: vec![("x4", false)], want: vec![], need_select: vec![], ranked: vec![] },
            Case { name: "good ones rank first, then by search; a shared frontier goes on once", spec: "", seed_se: None,
                rows: vec![row(1, 1, "x2", 2.0), row(2, 2, "x2", 2.0), row(3, 3, "xm1", -1.0), row(4, 4, "x1", 1.0)],
                selects: vec![], verdicts: vec![],
                want: vec![(1, "x2", true, None), (2, "x2", true, None), (3, "xm1", false, None), (4, "x1", true, None)],
                need_select: vec![], ranked: vec![1, 4, 3] },
        ];
        for case in cases {
            let spec = spec(case.spec);
            let seed = Seed { snap: "seed".into(), search: Some(0.0), se: case.seed_se, select: Some(0.0) };
            let claims: Vec<Claim> = (1..=4).map(claim).collect();
            let checks = Checks { selects: case.selects.iter().map(|(s, v)| (s.to_string(), *v)).collect(),
                verdicts: case.verdicts.iter().map(|(s, p)| (s.to_string(), (*p, String::new()))).collect() };
            let gated = gate(&spec, &seed, &case.rows, &claims, &checks);
            let got: Vec<(u32, &str, bool, Option<&str>)> = gated.decisions.iter()
                .map(|d| (d.idea, d.frontier.snap.as_str(), d.good, d.result.as_ref().map(|r| r.snap.as_str()))).collect();
            assert_eq!(got, case.want, "{}", case.name);
            assert_eq!(gated.need_select, case.need_select, "{}", case.name);
            assert_eq!(gated.ranked, case.ranked, "{}", case.name);
        }
    }

    #[test]
    fn audits_happen_when_a_candidate_first_becomes_good_and_at_the_last_gate() {
        let spec = spec("");
        let seed = Seed { snap: "seed".into(), search: Some(0.0), ..Seed::default() };
        let claims: Vec<Claim> = (1..=3).map(claim).collect();
        let rows = vec![row(1, 1, "x3", 3.0), row(2, 1, "x4", 4.0), row(3, 2, "x2", 2.0), row(4, 3, "xm1", -1.0)];
        let verdicts = |v: &[(&str, bool)]| Checks { verdicts: v.iter().map(|(s, p)| (s.to_string(), (*p, String::new()))).collect(), ..Checks::default() };
        let audited = |checks: &Checks, last| -> Vec<(u32, String)> {
            let gated = gate(&spec, &seed, &rows, &claims, checks);
            audits(&spec, &gated, last).iter().map(|d| (d.idea, d.frontier.snap.clone())).collect()
        };
        // New good candidates are audited, at their frontier; #3, below the seed, is not.
        assert_eq!(audited(&Checks::default(), false), vec![(1, "x4".to_string()), (2, "x2".to_string())]);
        // #1's refinement x4 waits while x3 passed; a failed state's candidate is audited at its next state.
        assert_eq!(audited(&verdicts(&[("x3", true), ("x2", false)]), false), vec![]);
        assert_eq!(audited(&verdicts(&[("x4", false), ("x2", true)]), false), vec![(1, "x3".to_string())]);
        // The last gate audits a lift beyond the margin over the passed result.
        assert_eq!(audited(&verdicts(&[("x3", true), ("x2", true)]), true), vec![(1, "x4".to_string())]);
        let strict = Spec { min_delta: 1.5, ..spec.clone() };
        let gated = gate(&strict, &seed, &rows, &claims, &verdicts(&[("x3", true), ("x2", true)]));
        assert!(audits(&strict, &gated, true).is_empty(), "a lift within the margin keeps the passed result unaudited");
    }

    #[test]
    fn the_portfolio_is_built_from_results() {
        let spec = spec("");
        let d = |idea: u32, frontier: f64, result: Option<(&str, f64)>| Decision { idea, claim: String::new(),
            frontier: Scored { snap: format!("f{idea}"), search: frontier, se: None, select: None }, good: true, audit: None,
            result: result.map(|(snap, search)| Scored { snap: snap.into(), search, se: None, select: None }) };
        let picked = portfolio(&spec, &[d(1, 9.0, Some(("a", 3.0))), d(2, 2.0, None), d(3, 5.0, Some(("b", 5.0))),
            d(4, 4.0, Some(("b", 5.0))), d(5, 1.0, Some(("c", 1.0))), d(6, 1.0, Some(("e", 0.5)))]);
        let ideas: Vec<u64> = picked.iter().map(|p| p["idea"].as_u64().unwrap()).collect();
        assert_eq!(ideas, vec![3, 1, 5], "best result first, one per state, at most three");
    }

    fn plan(label: &str, role: Role) -> Plan {
        Plan { label: label.into(), role, round: 1, idea: 0, start: "s".into(), best: None, seed: None }
    }

    #[test]
    fn interrupted_attempts_never_use_up_an_episode() {
        let started = |label: &str, plan: &str| Record::Started { label: label.into(), plan: plan.into(), scratch: None };
        let finished = |label: &str, evals, outcome| Record::Finished { label: label.into(), evals, outcome };
        let mut records = vec![];
        for (i, label) in ["r1-w1", "r1-w1.2", "r1-w1.3"].iter().enumerate() {
            records.push(started(label, "r1-w1"));
            records.push(finished(label, 0, Outcome::Interrupted));
            records.push(started(&format!("r0-research{}", if i == 0 { String::new() } else { format!(".{}", i + 1) }), "r0-research"));
            records.push(finished(&format!("r0-research{}", if i == 0 { String::new() } else { format!(".{}", i + 1) }), 0, Outcome::Interrupted));
        }
        let left = pending(vec![plan("r1-w1", Role::Explore), plan("r0-research", Role::Research)], &records, |_| false);
        assert_eq!(left.iter().map(|(_, l, c)| (l.as_str(), *c)).collect::<Vec<_>>(), vec![("r1-w1.4", 0), ("r0-research.4", 0)]);
        // A worker interrupted after scoring keeps its evals and is done; an auditor that recorded its verdict too.
        let records = vec![started("r1-w2", "r1-w2"), finished("r1-w2", 2, Outcome::Interrupted),
            started("a1-c1", "a1-c1"), finished("a1-c1", 0, Outcome::Interrupted)];
        assert!(pending(vec![plan("r1-w2", Role::Explore), plan("a1-c1", Role::Audit)], &records, |l| l == "a1-c1").is_empty());
        assert_eq!(pending(vec![plan("a1-c1", Role::Audit)], &records, |_| false).len(), 1);
        // Crashes count; a timed-out attempt is an outcome, not a crash.
        let records: Vec<Record> = ["r1-w3", "r1-w3.2", "r1-w3.3"].iter().flat_map(|l| [started(l, "r1-w3"), finished(l, 0, Outcome::Crashed)]).collect();
        assert!(pending(vec![plan("r1-w3", Role::Explore)], &records, |_| false).is_empty());
        let records = vec![started("r1-w4", "r1-w4"), finished("r1-w4", 0, Outcome::TimedOut)];
        assert!(pending(vec![plan("r1-w4", Role::Explore)], &records, |_| false).is_empty());
    }

    fn decision(idea: u32, search: f64, good: bool, se: Option<f64>) -> Decision {
        Decision { idea, claim: String::new(), frontier: Scored { snap: format!("s{idea}-{search}"), search, se, select: None },
            good, audit: None, result: None }
    }

    #[test]
    fn progress_needs_a_good_candidate_or_a_real_lift() {
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
    fn episodes_spawn_the_running_binary() {
        let exe = hx_exe().unwrap();
        if cfg!(target_os = "linux") { assert_eq!(exe, Path::new("/proc/self/exe")); }
        assert!(exe.exists());
    }

    #[test]
    fn templates_fill_every_placeholder() {
        assert_eq!(fill("a {{x}} {{y}} {{z}}\n[[on]] kept\n[[off]] dropped\n", &[("x", "{{y}}".into()), ("y", "2".into())], &["on"]),
            "a {{y}} 2 {{z}}\nkept\n");
        let dir = temp("tasks");
        let launch = Launch { format: FORMAT, spec: spec("select = \"s\"\n"), program: "PROGRAM".into(), child_args: vec![], commit: String::new() };
        let o = Orchestrator::new(&dir.join("ws"), dir.clone(), launch, hx_exe().unwrap());
        std::fs::write(dir.join("seed"), "seed").unwrap();
        let view = View { rows: vec![row(1, 1, "x3", 3.0)], claims: vec![claim(1)], brief: "BRIEF".into(), findings: String::new(),
            checks: Checks { verdicts: [("x3".to_string(), (false, "lookahead".to_string()))].into(), ..Checks::default() },
            seed: Seed::default(), plans: HashMap::new() };
        for role in [Role::Research, Role::Explore, Role::Exploit, Role::Audit] {
            let task = o.task(&Plan { idea: 1, seed: Some("SEED".into()), best: Some(3.0), ..plan("r1-w1", role) }, "r1-w1", &view).unwrap();
            assert!(!task.contains("{{") && !task.contains("[[") && task.starts_with("PROGRAM") && task.contains("<tools>"), "{role:?}: {task}");
            assert_eq!(task.contains("ar.verdict{"), role == Role::Audit, "{role:?}");
            assert!(task.contains("held-out data no agent sees"), "{role:?}");
        }
        let exploit = o.task(&Plan { idea: 1, ..plan("r2-w1", Role::Exploit) }, "r2-w1", &view).unwrap();
        assert!(exploit.contains("failed its audit: lookahead") && exploit.contains("candidate #1"), "{exploit}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn episode_labels() {
        for ok in ["r0-research", "r1-w3", "r1-w3.2", "a2-c5", "f4-c1"] { assert!(valid_label(ok), "{ok}"); }
        for bad in ["", "../x", "r1/w1", "r1-w1.", "r1-w1.x", ".2", "r1-w1.2.3"] { assert!(!valid_label(bad), "{bad}"); }
    }

    #[test]
    fn jsonl_reads_incrementally_and_appends_drop_a_torn_line() {
        let dir = temp("tail");
        let path = dir.join("log.jsonl");
        let mut tail = Tail::<Value>::new(path.clone());
        assert!(tail.read().unwrap().is_empty());
        append_line(&path, &json!({"n": 1})).unwrap();
        std::fs::OpenOptions::new().append(true).open(&path).and_then(|mut f| std::io::Write::write_all(&mut f, b"{\"n\": ")).unwrap();
        assert_eq!(tail.read().unwrap(), &[json!({"n": 1})]);
        append_line(&path, &json!({"n": 2})).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"n\":1}\n{\"n\":2}\n");
        assert_eq!(tail.read().unwrap(), &[json!({"n": 1}), json!({"n": 2})]);
        assert_eq!(read_lines::<Value>(&path).unwrap().len(), 2);
        // A file replaced by a shorter one is read again.
        std::fs::write(&path, "{\"n\":3}\n").unwrap();
        assert_eq!(tail.read().unwrap(), &[json!({"n": 3})]);
        std::fs::write(&path, "{\"n\":3}\nnot json\n").unwrap();
        assert!(tail.read().is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn children_get_no_held_out_details_and_unresolved_roots_stop_the_run() {
        let dir = temp("child");
        let (grader, holdout) = (dir.join("grader"), dir.join("holdout"));
        std::fs::create_dir_all(&grader).unwrap();
        std::fs::create_dir_all(&holdout).unwrap();
        let run = dir.join("state/ar/run");
        std::fs::create_dir_all(&run).unwrap();
        let spec = spec(&format!("select = \"python3 secret_select.py\"\ngrader = [\"{}\"]\nholdout = [\"{}\"]\n", grader.display(), holdout.display()));
        let launch = Launch { format: FORMAT, spec, program: String::new(), child_args: vec![], commit: String::new() };
        let o = Orchestrator::new(&dir.join("ws"), run.clone(), launch, hx_exe().unwrap());
        let plan = plan("r1-w1", Role::Explore);
        let episode = o.child_episode(&plan, "r1-w1").unwrap();
        let text = serde_json::to_string(&episode.spec).unwrap();
        assert!(!text.contains("secret_select") && !text.contains("holdout\":[\"") && episode.spec.grader.len() == 1, "{text}");
        assert_eq!(sealed(&episode), vec![dir.join("state/ar"), grader.clone(), holdout.clone()]);
        std::fs::remove_dir_all(&holdout).unwrap();
        assert!(o.child_episode(&plan, "r1-w1").is_err(), "an unresolved holdout root must not go unsealed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_run_is_locked_while_an_orchestrator_holds_it() {
        let dir = temp("lock");
        let held = lock_run(&dir).unwrap();
        assert!(lock_run(&dir).unwrap_err().to_string().contains("in use"));
        drop(held);
        assert!(lock_run(&dir).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
