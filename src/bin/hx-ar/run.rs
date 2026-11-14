//! The autoresearch run: a manager loop that improves a score by changing a task's editable files, and returns a
//! portfolio of distinct good candidates rather than one best state.
//!
//! The task supplies `.harness/ar.toml` (editable paths, evaluator commands, grader and holdout roots) and the program
//! text (`--prompt-file`). At launch the committed tree is frozen into the run's `base`. Episodes work in copies of the
//! owner's workspace; on Linux, Landlock confines their processes to their files (network isolation is external):
//!
//! - Every episode is a fresh child `hx run` in a throwaway copy of `base` holding the state it starts from. Only
//!   snapshots of the editable paths ever leave an episode, and only this process takes them.
//! - The agent calls `mcp.ar.eval`, `mcp.ar.claim`, `mcp.ar.claims` and `mcp.ar.reset`: tools of an MCP server this
//!   process serves on loopback (`crate::serve`), with a bearer token per episode. `eval` snapshots the episode's
//!   editable files and scores the snapshot in a second throwaway copy of `base`, so the evaluator sees exactly the
//!   recorded bytes and nothing else the agent did.
//! - Evaluators run in this process, with a scrubbed environment in a throwaway copy, confined (Landlock) to that copy,
//!   the system and their grader roots; only the select evaluator may read the holdout roots.
//! - The run directory (`RUN` in the workspace's autoresearch directory, with `base`, snapshots, the journal and the
//!   episodes' records) is in hx's state directory, outside every root of every episode. This process is its only
//!   writer, and the run's state is a fold over its journal (`Journal`).
//!
//! A run is research, then rounds. A research episode studies the task and writes the brief every worker gets. Round 1
//! runs explore workers side by side; each derives its own idea from a host seed and claims it with `claim`. Claims
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
//! `min_delta` and the combined standard error of the two scores, and two rounds in a row without progress end the
//! run. With no valid candidate after round 1 the run stops. The result is the audited good candidates of the last
//! gate, best search score first: several distinct good answers, since the single best score is the one most likely to
//! be an overfit or an evaluator hole.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::files::*;
use crate::serve::{Ep, Shared};
use hx::dirs::Dirs;
use std::sync::Arc;

/// Workers side by side in a round, and fewer once the provider has rate-limited an episode of the run.
const WORKERS: usize = 8;
const NARROW: usize = 6;
/// The environment variable that carries an episode's bearer token to its child `hx run` (`bearer_env` in its
/// `mcp.toml`). Shell commands never see it: hx scrubs their environment.
const TOKEN_ENV: &str = "HX_AR_TOKEN";
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
pub(crate) struct Spec {
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
    /// The margin, besides their combined standard error, by which a candidate's search score must beat the seed's
    /// for the candidate to be good, and by which a chosen candidate's must beat its score at the last progress for a
    /// round to make progress. A worker's state improves on its best by more than their combined standard error alone.
    #[serde(default)]
    pub min_delta: f64,
}
/// Which way the score improves, stored as `"max"` or `"min"` in `ar.toml` and the frozen `launch.json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Max,
    Min,
}

impl Spec {
    fn timeout() -> f64 {
        600.0
    }
    /// The spec in `config`, the workspace config the trust decision covered (never a second read of the file). No
    /// grader or holdout root may overlap the workspace or hx's own directories (`dirs`), or lie where the agent's
    /// shell commands may read.
    pub(crate) fn load(workspace: &Path, config: &hx::trust::Config, dirs: &Dirs) -> Result<Spec> {
        let path = workspace.join(".harness/ar.toml");
        let text = config
            .file(".harness/ar.toml")
            .with_context(|| format!("hx-ar needs {}", path.display()))?;
        let spec: Spec = std::str::from_utf8(text)
            .map_err(anyhow::Error::from)
            .and_then(|t| toml::from_str(t).map_err(anyhow::Error::from))
            .with_context(|| format!("invalid {}", path.display()))?;
        ensure!(
            !spec.editable.is_empty(),
            "ar.toml: editable must name at least one path"
        );
        for p in &spec.editable {
            let parts: Vec<&str> = p.trim_end_matches('/').split('/').collect();
            ensure!(
                !p.is_empty()
                    && !Path::new(p).is_absolute()
                    && parts
                        .iter()
                        .all(|c| !c.is_empty() && *c != "." && *c != "..")
                    && !matches!(parts[0], ".harness" | ".git")
                    // the workspace's skills are trusted configuration that every episode loads
                    && !(parts[0] == ".agents" && parts.get(1).is_none_or(|c| *c == "skills")),
                "ar.toml: editable path {p:?} must be a relative path inside the workspace, outside .harness, .git and .agents/skills"
            );
        }
        let agent_reads = hx::policy::system_read_roots();
        for g in spec.roots(true)? {
            ensure!(
                !g.starts_with(workspace) && !workspace.starts_with(&g),
                "ar.toml: grader and holdout root {} must not overlap the workspace",
                g.display()
            );
            // Model-edited code runs in the evaluators, which read these roots.
            if let Some((what, dir)) = dirs.overlap(&g) {
                bail!(
                    "ar.toml: grader and holdout root {} overlaps hx's {what} directory {}",
                    g.display(),
                    dir.display()
                );
            }
            // Landlock only adds access, so no root of an episode's shell commands may hold one.
            if let Some(r) = agent_reads.iter().find(|r| g.starts_with(r)) {
                bail!(
                    "ar.toml: grader and holdout root {} lies inside {}, which the agent's shell commands may read",
                    g.display(),
                    r.display()
                );
            }
        }
        search_read_roots(&spec)?;
        ensure!(
            spec.timeout > 0.0 && spec.min_delta >= 0.0,
            "ar.toml: timeout must be positive and min_delta nonnegative"
        );
        Ok(spec)
    }
    /// Read roots of an evaluator: `grader`, plus `holdout` for the select evaluator. With `holdout`, also the roots an
    /// episode's file tools may never reach.
    pub(crate) fn roots(&self, holdout: bool) -> Result<Vec<PathBuf>> {
        let extra = if holdout { &self.holdout[..] } else { &[] };
        self.grader.iter().chain(extra).map(|g| expand(g)).collect()
    }
    /// The `holdout` roots alone.
    pub(crate) fn holdout_roots(&self) -> Result<Vec<PathBuf>> {
        self.holdout.iter().map(|g| expand(g)).collect()
    }
    /// Is `a` better than `b` by more than `margin`?
    pub(crate) fn better(&self, a: f64, b: Option<f64>, margin: f64) -> bool {
        match b {
            None => true,
            Some(b) if self.direction == Direction::Max => a > b + margin,
            Some(b) => a < b - margin,
        }
    }
    /// Is the state estimated by `a` better than the one estimated by `b` by more than `floor` and more than their
    /// combined standard error? Without a `b` any valid state is.
    pub(crate) fn beats(&self, a: Estimate, b: Option<Estimate>, floor: f64) -> bool {
        b.is_none_or(|b| self.better(a.mean, Some(b.mean), floor.max(a.combined_se(b))))
    }
}

/// A root path of ar.toml, with `~` and `$HOME` (alone or before `/`) expanded, canonical.
fn expand(root: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let expanded = match root {
        "~" | "$HOME" => home,
        _ => root
            .strip_prefix("~/")
            .or_else(|| root.strip_prefix("$HOME/"))
            .map_or_else(|| root.to_string(), |rest| format!("{home}/{rest}")),
    };
    std::fs::canonicalize(&expanded).with_context(|| format!("ar.toml: root {root}"))
}

/// What the evaluations of one state say about its score: their mean, and the standard error of that mean when the
/// evaluator reports one (none: a deterministic evaluator, whose repeated evaluations agree). Evaluating the same
/// bytes again refines the estimate; it is not a new state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Estimate {
    pub(crate) mean: f64,
    pub(crate) se: Option<f64>,
}
impl Estimate {
    /// The estimate from valid evaluations `(score, se)`; none without any. An evaluation without `se` counts as
    /// exact.
    fn of(samples: impl IntoIterator<Item = (f64, Option<f64>)>) -> Option<Estimate> {
        let (mut n, mut sum, mut var, mut noisy) = (0.0, 0.0, 0.0, false);
        for (score, se) in samples {
            n += 1.0;
            sum += score;
            var += se.unwrap_or(0.0).powi(2);
            noisy |= se.is_some();
        }
        (n > 0.0).then(|| Estimate {
            mean: sum / n,
            se: noisy.then(|| var.sqrt() / n),
        })
    }
    /// The standard error of the difference between two estimates; 0 when both are exact.
    fn combined_se(self, other: Estimate) -> f64 {
        (self.se.unwrap_or(0.0).powi(2) + other.se.unwrap_or(0.0).powi(2)).sqrt()
    }
}

/// The estimate of `snap`'s search score from the journal's valid evaluations of it, and from the host's baseline
/// evaluation when `snap` is the seed.
pub(crate) fn estimate(records: &[Record], snap: &str) -> Option<Estimate> {
    estimate_of(rows(records), records, snap)
}

/// `estimate` from the evaluations `rows` (the journal's, and one about to be journaled).
pub(crate) fn estimate_of<'a>(
    rows: impl Iterator<Item = &'a Row>,
    records: &[Record],
    snap: &str,
) -> Option<Estimate> {
    let samples = rows
        .filter(|r| r.snap == snap && r.valid)
        .filter_map(|r| Some((r.score?, r.se)));
    let baseline = (seed(records) == Some(snap))
        .then(|| baseline(records))
        .flatten()
        .and_then(|b| Some((b.score?, b.se)));
    Estimate::of(samples.chain(baseline))
}

/// Whether the search evaluator is noisy: it has reported `se` for some evaluation of this run (the host's baseline
/// included). A noisy evaluator's result is never reused for the same state, even one it scored without `se`.
pub(crate) fn noisy(records: &[Record]) -> bool {
    rows(records).any(|r| r.se.is_some()) || baseline(records).is_some_and(|b| b.se.is_some())
}

/// The host's search evaluation of the seed, journaled before any episode starts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Baseline {
    score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se: Option<f64>,
    #[serde(default)]
    report: Value,
}

/// The evaluations journaled so far, in order.
pub(crate) fn rows(records: &[Record]) -> impl Iterator<Item = &Row> {
    records.iter().filter_map(|r| match r {
        Record::Evaluated(row) => Some(row),
        _ => None,
    })
}

/// The claims accepted so far, in order.
pub(crate) fn claims(records: &[Record]) -> impl Iterator<Item = &Claim> {
    records.iter().filter_map(|r| match r {
        Record::Claimed(claim) => Some(claim),
        _ => None,
    })
}

/// The seed's snapshot, journaled at launch.
fn seed(records: &[Record]) -> Option<&str> {
    records.iter().find_map(|r| match r {
        Record::Seed { snap } => Some(snap.as_str()),
        _ => None,
    })
}

fn baseline(records: &[Record]) -> Option<&Baseline> {
    records.iter().find_map(|r| match r {
        Record::Baseline(b) => Some(b),
        _ => None,
    })
}

/// What an episode is for. Research writes the brief; explore workers claim and test a new idea; exploit workers claim
/// and test a refinement of one candidate; auditors judge whether a candidate's score is real. Research and audit
/// evaluations are probes: recorded, never a candidate's state.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Role {
    Research,
    Explore,
    Exploit,
    Audit,
}

/// One evaluation an episode asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Row {
    pub(crate) n: usize,
    pub(crate) episode: String,
    /// The candidate this state belongs to; 0 for research and audit probes.
    #[serde(default)]
    pub(crate) idea: u32,
    pub(crate) note: String,
    pub(crate) snap: String,
    pub(crate) score: Option<f64>,
    pub(crate) valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) se: Option<f64>,
    pub(crate) improved: bool,
    /// The search evaluator's report, shown to later episodes for a candidate's best state.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub(crate) report: Value,
}

/// One accepted claim. Explore claims share the scope `explore` and their number is the candidate's id; an exploit
/// worker's refinement claims are scoped to its candidate (`c3`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Claim {
    pub(crate) scope: String,
    pub(crate) n: u32,
    pub(crate) idea: String,
    pub(crate) worker: String,
}

/// The run's journal, `journal.jsonl`: one JSON record per line, every plan, evaluation, claim and verdict. This
/// process is its only writer. It holds the file locked while it lives (a second `hx-ar` on the same run is refused)
/// and keeps the records in memory, so the run's state is a fold over them and is never read back from disk.
pub(crate) struct Journal {
    inner: std::sync::Mutex<(std::fs::File, Vec<Record>)>,
}

impl Journal {
    /// Open the journal of the run in `dir`, creating it, and lock it. A final record cut off when the process
    /// writing it was killed is dropped; damage anywhere else is an error.
    pub(crate) fn open(dir: &Path) -> Result<Journal> {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        let path = dir.join("journal.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("journal {}", path.display()))?;
        // SAFETY: flock(2) on a descriptor this function owns.
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "autoresearch run {} is in use by another hx-ar",
            dir.display()
        );
        let mut text = Vec::new();
        file.read_to_end(&mut text)?;
        let complete = text.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if complete < text.len() {
            file.set_len(complete as u64)?;
        }
        let records = std::str::from_utf8(&text[..complete])
            .with_context(|| format!("{} is not UTF-8", path.display()))?
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| {
                serde_json::from_str(l)
                    .with_context(|| format!("{} line {}", path.display(), i + 1))
            })
            .collect::<Result<_>>()?;
        Ok(Journal {
            inner: std::sync::Mutex::new((file, records)),
        })
    }

    /// The records so far.
    pub(crate) fn records(&self) -> Vec<Record> {
        self.inner.lock().expect("journal lock poisoned").1.clone()
    }

    /// Append one record durably.
    pub(crate) fn append(&self, record: Record) -> Result<()> {
        self.append_with(|_| Ok((record, ())))
    }

    /// Append the record `make` builds from the records so far, and return what it returns besides: one step, so that
    /// numbering or judging against earlier records never interleaves with another append.
    pub(crate) fn append_with<T>(
        &self,
        make: impl FnOnce(&[Record]) -> Result<(Record, T)>,
    ) -> Result<T> {
        use std::io::Write as _;
        let mut inner = self.inner.lock().expect("journal lock poisoned");
        let (file, records) = &mut *inner;
        let (record, out) = make(records)?;
        writeln!(file, "{}", serde_json::to_string(&record)?)?;
        file.sync_data()?;
        records.push(record);
        Ok(out)
    }
}

/// What the two judge calls of one claim spent, as the provider reported it. These calls are made by hx-ar, outside
/// every episode's run and budget, so the run's journal is their only record. A call that failed reported nothing;
/// one whose answer was lost may still have been billed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct JudgeSpend {
    /// The worker whose claim was judged, and the claim.
    pub(crate) worker: String,
    pub(crate) idea: String,
    /// `new`, `same as #N` or `placeholder`.
    pub(crate) verdict: String,
    pub(crate) model: String,
    pub(crate) calls: u32,
    pub(crate) failed: u32,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
}

/// The judge's spend over the whole run, for the report.
fn judge_totals(records: &[Record]) -> Value {
    let mut total = JudgeSpend::default();
    for r in records {
        if let Record::Judged(s) = r {
            total.calls += s.calls;
            total.failed += s.failed;
            total.input_tokens += s.input_tokens;
            total.output_tokens += s.output_tokens;
            total.cache_read_tokens += s.cache_read_tokens;
            total.cache_write_tokens += s.cache_write_tokens;
        }
    }
    json!({"calls": total.calls, "failed": total.failed, "input_tokens": total.input_tokens,
        "output_tokens": total.output_tokens, "cache_read_tokens": total.cache_read_tokens,
        "cache_write_tokens": total.cache_write_tokens})
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
pub(crate) struct Plan {
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
pub(crate) struct Candidate {
    idea: u32,
    claim: String,
    snap: String,
    search: Option<f64>,
    /// The standard error of `search`, when the evaluator reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    se: Option<f64>,
    select: Option<f64>,
    good: bool,
    audit: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    reason: String,
}
impl Candidate {
    /// The estimate of its best state's search score, as the gate saw it.
    fn estimate(&self) -> Option<Estimate> {
        self.search.map(|mean| Estimate { mean, se: self.se })
    }
}

/// Whether a round that chose `chosen` made progress since the round that last did, which left `at` (each chosen
/// candidate with its estimate then): a candidate is new, or one beats its estimate then by more than `min_delta` and
/// their combined standard error, the rule the gate uses against the seed. Re-scoring a state moves its mean but
/// rarely by that much, so a candidate whose files did not change almost never makes progress.
fn progress(spec: &Spec, chosen: &[&Candidate], at: &[(u32, Option<Estimate>)]) -> bool {
    chosen
        .iter()
        .any(|c| match at.iter().find(|(i, _)| *i == c.idea) {
            None => true,
            Some((_, before)) => c
                .estimate()
                .is_some_and(|now| spec.beats(now, *before, spec.min_delta)),
        })
}

/// Journal records. An episode is `started` before its child runs and `finished` after; resume closes one that never
/// finished (as finished when its child reported, as interrupted otherwise), runs what was planned but not finished,
/// and continues. Held-out scores, audit verdicts and gates are journaled once and reused; evaluations and claims are
/// journaled as the episodes make them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Record {
    /// The snapshot of the owner's editable files at launch.
    Seed {
        snap: String,
    },
    /// The host's search evaluation of the seed.
    Baseline(Baseline),
    /// An evaluation an episode asked for.
    Evaluated(Row),
    /// A claim accepted for a worker.
    Claimed(Claim),
    /// What the judge calls of one claim spent.
    Judged(JudgeSpend),
    Planned {
        batch: String,
        episodes: Vec<Plan>,
    },
    Started {
        label: String,
        plan: String,
        #[serde(default)]
        scratch: Option<PathBuf>,
    },
    /// `crashed`: it ended without scoring anything or giving a final answer, and is retried. `interrupted`: the
    /// orchestrator stopped while it ran; such an attempt is retried without counting as a crash.
    Finished {
        label: String,
        #[serde(default)]
        interrupted: bool,
        #[serde(default)]
        crashed: bool,
        /// Requests the provider refused with HTTP 429, from the child's report.
        #[serde(default)]
        rate_limits: u64,
    },
    /// Every attempt of every episode of `batch` crashed, so the run stopped there; a resumed run tries the batch's
    /// unfinished episodes again with fresh attempts.
    Stalled {
        batch: String,
    },
    Selected {
        snap: String,
        score: Option<f64>,
        #[serde(default)]
        se: Option<f64>,
    },
    Audited {
        idea: u32,
        snap: String,
        pass: bool,
        reason: String,
    },
    Gate {
        round: usize,
        candidates: Vec<Candidate>,
        chosen: Vec<u32>,
    },
}

/// What one episode did.
struct Outcome {
    crashed: bool,
    stderr: String,
}

struct Orchestrator {
    /// hx's directories; episode children share them, so their journals land in this state directory.
    dirs: Dirs,
    workspace: PathBuf,
    dir: PathBuf,
    launch: Launch,
    /// What the episodes' MCP server shares with this loop: the journal, the spec, the episodes being served.
    shared: Arc<Shared>,
}

impl Orchestrator {
    fn spec(&self) -> &Spec {
        &self.launch.spec
    }

    fn journal(&self, record: &Record) -> Result<()> {
        self.shared.journal.append(record.clone())
    }

    fn records(&self) -> Result<Vec<Record>> {
        Ok(self.shared.journal.records())
    }

    fn seed(&self) -> Result<String> {
        seed(&self.records()?)
            .map(String::from)
            .context("the journal has no seed")
    }

    /// Score the seed with the search evaluator before any episode, so every episode knows where it starts. Journaled
    /// apart from the agents' evaluations.
    async fn baseline(&self) -> Result<()> {
        let scored = evaluate(
            self.spec(),
            &self.dir,
            &load_snap(&self.dir, &self.seed()?)?,
            &self.spec().eval,
            false,
        )
        .await?;
        eprintln!(
            "hx-ar: seed scores {}",
            if scored.valid {
                show(scored.score)
            } else {
                "invalid".into()
            }
        );
        let score = scored.score.filter(|_| scored.valid);
        let baseline = Baseline {
            score,
            se: scored.se.filter(|_| score.is_some()),
            report: scored.report,
        };
        self.journal(&Record::Baseline(baseline))
    }

    /// The search score of `snap` (see `estimate`) and its report: that of its first valid evaluation, or the host's
    /// baseline report for the seed.
    fn known(&self, records: &[Record], snap: &str) -> (Option<Estimate>, Value) {
        let report = match rows(records).find(|r| r.snap == snap && r.valid) {
            Some(r) => r.report.clone(),
            None if seed(records) == Some(snap) => {
                baseline(records).map_or(Value::Null, |b| b.report.clone())
            }
            None => Value::Null,
        };
        (estimate(records, snap), report)
    }

    /// The held-out score of `snap` (none without a select evaluator, or when it is invalid), journaled once.
    async fn select(&self, snap: &str) -> Result<Option<Estimate>> {
        let Some(cmd) = self.spec().select.clone() else {
            return Ok(None);
        };
        let journaled = self.records()?.into_iter().find_map(|r| match r {
            Record::Selected { snap: s, score, se } if s == snap => Some((score, se)),
            _ => None,
        });
        let (score, se) = match journaled {
            Some(known) => known,
            None => {
                let scored = evaluate(
                    self.spec(),
                    &self.dir,
                    &load_snap(&self.dir, snap)?,
                    &cmd,
                    true,
                )
                .await?;
                let score = scored.score.filter(|_| scored.valid);
                let se = scored.se.filter(|_| score.is_some());
                self.journal(&Record::Selected {
                    snap: snap.into(),
                    score,
                    se,
                })?;
                (score, se)
            }
        };
        Ok(score.map(|mean| Estimate { mean, se }))
    }

    /// The final answer of the last finished attempt of a planned episode: a worker's finding, the brief, a verdict.
    fn answer(&self, records: &[Record], plan: &str) -> Option<String> {
        let label = records.iter().rev().find_map(|r| match r {
            Record::Started { label, plan: p, .. } if p == plan => Some(label.clone()),
            _ => None,
        })?;
        let report: Value = serde_json::from_slice(
            &std::fs::read(
                self.dir
                    .join("episodes")
                    .join(format!("{label}.report.json")),
            )
            .ok()?,
        )
        .ok()?;
        report["final_answer"]
            .as_str()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(String::from)
    }

    /// Workers per round: `WORKERS`, or `NARROW` once the provider has refused a request of any episode with HTTP 429.
    fn width(&self) -> Result<usize> {
        let limited = self
            .records()?
            .iter()
            .any(|r| matches!(r, Record::Finished { rate_limits, .. } if *rate_limits > 0));
        Ok(if limited { NARROW } else { WORKERS })
    }

    /// The episodes of a journaled batch.
    fn planned(&self, batch: &str) -> Result<Option<Vec<Plan>>> {
        Ok(self.records()?.into_iter().find_map(|r| match r {
            Record::Planned { batch: b, episodes } if b == batch => Some(episodes),
            _ => None,
        }))
    }

    /// Journal a batch once.
    fn plan(&self, batch: &str, episodes: Vec<Plan>) -> Result<()> {
        if !self
            .records()?
            .iter()
            .any(|r| matches!(r, Record::Planned { batch: b, .. } if b == batch))
        {
            self.journal(&Record::Planned {
                batch: batch.into(),
                episodes,
            })?;
        }
        Ok(())
    }

    /// Run a planned batch side by side until every episode has finished, or crashed `MAX_CRASHES` times. When every
    /// attempt of every episode crashed (the provider is down, or the run is misconfigured), the run stops with an
    /// error rather than going on as if the batch had found nothing; `--resume` then tries the batch again.
    async fn run_batch(&mut self, batch: &str) -> Result<()> {
        let (mut crashes, mut last_error) = (0, String::new());
        loop {
            let records = self.records()?;
            let plans = self.planned(batch)?.unwrap_or_default();
            let tries: Vec<Attempts> = plans
                .iter()
                .map(|plan| attempts(&records, batch, &plan.label))
                .collect();
            let pending: Vec<(&Plan, String)> = plans
                .iter()
                .zip(&tries)
                .filter(|(_, t)| !t.succeeded && t.crashes < MAX_CRASHES)
                .map(|(plan, t)| {
                    (
                        plan,
                        if t.started == 0 {
                            plan.label.clone()
                        } else {
                            format!("{}.{}", plan.label, t.started + 1)
                        },
                    )
                })
                .collect();
            if pending.is_empty() {
                if plans.is_empty() || tries.iter().any(|t| t.succeeded) {
                    return Ok(());
                }
                self.journal(&Record::Stalled {
                    batch: batch.into(),
                })?;
                bail!(
                    "the episodes of {batch} keep failing: every one crashed {MAX_CRASHES} times; last error:\n{last_error}\n\
                    Fix the cause, then continue with `hx-ar --resume {}`",
                    self.dir.display()
                );
            }
            let retries_left = pending.iter().any(|(plan, _)| {
                plans
                    .iter()
                    .position(|p| p.label == plan.label)
                    .is_some_and(|i| tries[i].crashes + 1 < MAX_CRASHES)
            });
            let state = self.state()?;
            let mut runs = vec![];
            for (plan, label) in &pending {
                runs.push(self.episode(plan, label, self.task(plan, label, &state)?));
            }
            let outcomes = futures::future::join_all(runs)
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            if !outcomes.iter().all(|o| o.crashed) {
                crashes = 0;
                continue;
            }
            crashes += 1;
            let tail: Vec<&str> = outcomes[0].stderr.trim().lines().rev().take(15).collect();
            last_error = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
            if !retries_left {
                continue;
            }
            let base = std::env::var("HX_AR_RETRY_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            let pause = base << crashes;
            eprintln!(
                "hx-ar: every episode of {batch} crashed before any eval; retrying in {pause}s"
            );
            tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
        }
    }

    /// One fresh child run in its own copy of the base holding its starting state, configured to reach this process's
    /// MCP server with a token that names the episode, and trusting exactly that configuration.
    async fn episode(&self, plan: &Plan, label: &str, task: String) -> Result<Outcome> {
        let ep_dir = self.dir.join("episodes");
        std::fs::create_dir_all(&ep_dir)?;
        let task_path = ep_dir.join(format!("{label}.md"));
        std::fs::write(&task_path, task)?;
        let (dir, spec, start) = (self.dir.clone(), self.spec().clone(), plan.start.clone());
        let url = self.shared.url()?;
        let scratch = tokio::task::spawn_blocking(move || -> Result<Scratch> {
            let scratch = Scratch::new("ws")?;
            let ws = scratch.0.join("ws");
            copy_tree(&dir.join("base"), &ws)?;
            place(&ws, &spec, &load_snap(&dir, &start)?)?;
            configure(&ws, &spec, &url)?;
            Ok(scratch)
        })
        .await??;
        let ws = scratch.0.join("ws");
        let trust = hx::trust::config_hash(&ws).context("the episode's configuration")?;
        self.journal(&Record::Started {
            label: label.into(),
            plan: plan.label.clone(),
            scratch: Some(scratch.0.clone()),
        })?;
        eprintln!(
            "hx-ar: {label} ({:?}{})",
            plan.role,
            if plan.idea > 0 {
                format!(" #{}", plan.idea)
            } else {
                String::new()
            }
        );
        let token = hx::tools::nonce()?;
        // The token is served until the child has exited; a call made with it after that is refused.
        let _served = self.shared.serve(
            token.clone(),
            Ep {
                label: label.into(),
                plan: plan.label.clone(),
                role: plan.role,
                idea: plan.idea,
                start: plan.start.clone(),
                ws: ws.clone(),
            },
        );
        let mut command = tokio::process::Command::new(hx_binary()?);
        command
            .arg("run")
            .arg("--workspace")
            .arg(&ws)
            .args(&self.launch.child_args)
            .arg("--prompt-file")
            .arg(&task_path)
            .arg("--max-turns")
            .arg(TURNS.to_string())
            .env("HX_TRUST", trust)
            .env(TOKEN_ENV, &token)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        stop_with_this_process(&mut command);
        let child = command.spawn().context("cannot start hx")?;
        // A signal to this process asks the child to stop (`hx::procs::stop`), and it stops its own commands.
        let tracked = child.id().map(hx::procs::Child::track);
        let out = child.wait_with_output().await?;
        drop(tracked);
        std::fs::write(ep_dir.join(format!("{label}.stderr")), &out.stderr)?;
        let report = String::from_utf8_lossy(&out.stdout)
            .lines()
            .rev()
            .find_map(|l| serde_json::from_str::<Value>(l).ok())
            .unwrap_or(Value::Null);
        std::fs::write(
            ep_dir.join(format!("{label}.report.json")),
            serde_json::to_vec_pretty(&report)?,
        )?;
        let logs = keep_logs(&self.dirs.workspace(&ws).log, &ep_dir, label)?;
        drop(scratch);
        // Without a report the child's journals say how often it was rate-limited.
        let rate_limits = report["rate_limits"]
            .as_u64()
            .unwrap_or_else(|| rate_limited(&logs));
        let evals = rows(&self.records()?)
            .filter(|r| r.episode == label)
            .count();
        let crashed = evals == 0 && report.is_null();
        self.journal(&Record::Finished {
            label: label.into(),
            interrupted: false,
            crashed,
            rate_limits,
        })?;
        let end = if report["finished"] == true {
            "finished"
        } else {
            report["stopped"].as_str().unwrap_or("stopped")
        };
        eprintln!(
            "hx-ar: {label}: {evals} evals, {end}{}",
            if rate_limits > 0 {
                format!(", {rate_limits} rate-limited")
            } else {
                String::new()
            }
        );
        Ok(Outcome {
            crashed,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn state(&self) -> Result<State> {
        let records = self.records()?;
        let rows: Vec<Row> = rows(&records).cloned().collect();
        let claims: Vec<Claim> = claims(&records).cloned().collect();
        let brief = self
            .answer(&records, "r0-research")
            .unwrap_or_else(|| "(none: the research episode wrote no brief)".into());
        // A finding per finished worker attempt: its claim, best score and final answer; the most recent are shown.
        let mut findings = vec![];
        for r in &records {
            let Record::Finished {
                label,
                crashed: false,
                ..
            } = r
            else {
                continue;
            };
            if !label.starts_with('r') || label.starts_with("r0") {
                continue;
            }
            let claim = claims.iter().rev().find(|c| c.worker == *label).map_or(
                "(no accepted claim)".to_string(),
                |c| match c.scope.as_str() {
                    "explore" => format!("idea #{}: {}", c.n, c.idea),
                    scope => format!("refinement of #{}: {}", &scope[1..], c.idea),
                },
            );
            // Each state that improved a worker beat its best, so the last one is its best.
            let best = rows
                .iter()
                .rev()
                .find(|x| x.episode == *label && x.improved)
                .and_then(|x| estimate(&records, &x.snap))
                .map(|e| e.mean);
            let text = std::fs::read(
                self.dir
                    .join("episodes")
                    .join(format!("{label}.report.json")),
            )
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["final_answer"].as_str().map(String::from))
            .unwrap_or_default();
            findings.push(format!(
                "### {label}: {} (best {})\n\n{}\n\n",
                claim.replace('\n', " "),
                show(best),
                clip(text.trim(), FINDING_CHARS)
            ));
        }
        let skip = findings.len().saturating_sub(FINDINGS);
        let findings = if skip > 0 {
            format!("({skip} earlier findings omitted)\n\n")
        } else {
            String::new()
        } + &findings[skip..].concat();
        Ok(State {
            records,
            rows,
            claims,
            brief,
            findings,
        })
    }

    /// The tools an episode has, in the order the prompt guide asks for: grounding before the task, rules last.
    fn tools(&self, role: Role) -> String {
        let spec = self.spec();
        let better = if spec.direction == Direction::Max {
            "higher"
        } else {
            "lower"
        };
        let mut t = format!(
            "<tools>\n- `mcp.ar.eval{{note = \"one-line note\"}}` scores the editable files ({}) with the owner's frozen evaluator \
and returns {{n, score, valid, se, improved, best, report}}; {better} scores are better. Only states scored by mcp.ar.eval \
count. A deterministic evaluator (one that has reported no `se` in this run) returns a state it already scored from its \
cache.\n- `mcp.ar` is the run's MCP server, bound like `sh` and `read`; a refused call raises an error (`pcall` it to \
read the reason).\n",
            spec.editable.join(", ")
        );
        if matches!(role, Role::Explore | Role::Exploit) {
            t += "- `mcp.ar.claims{}` lists the claims yours must differ from. `mcp.ar.claim{idea = text}` claims your idea or \
refinement.\n- `improved` means a new state better than your best so far (by more than the combined `se` when the \
evaluator reports one; scoring the same files again refines their score, it never improves). `mcp.ar.reset{}` puts \
your best scored state back into the editable paths.\n";
        } else {
            t += "- Your evaluations are probes: recorded, never a candidate. Change the editable files freely to test a question.\n";
        }
        if spec.select.is_some() {
            t += "- Results are judged on held-out data no agent sees: a change with a reason to generalize beats tuning to the reported numbers.\n";
        }
        t + &format!(
            "- You have {TURNS} steps (model turns). At the limit the run stops you, with or without a final answer.\n</tools>\n\n"
        )
    }

    fn task(&self, plan: &Plan, label: &str, state: &State) -> Result<String> {
        let spec = self.spec();
        let (score, report) = self.known(&state.records, &plan.start);
        let score = score.map(|e| e.mean);
        let report = clip(
            &serde_json::to_string_pretty(&report).unwrap_or_default(),
            REPORT_CHARS,
        );
        let mut context = format!("{}\n\n", self.launch.program.trim_end());
        if plan.role != Role::Research {
            context += &format!("<brief>\n{}\n</brief>\n\n", clip(&state.brief, BRIEF_CHARS));
            if !state.findings.is_empty() {
                context += &format!("<findings>\n{}</findings>\n\n", state.findings);
            }
        }
        context += &format!(
            "<starting_state>\nYour workspace holds it (search score {}). Its evaluator report:\n```json\n{report}\n```\n</starting_state>\n\n",
            show(score)
        );
        let claim = |idea: u32| {
            state
                .claims
                .iter()
                .find(|c| c.scope == "explore" && c.n == idea)
                .map_or(String::new(), |c| c.idea.clone())
        };
        let worker_rules = "- mcp.ar.eval refuses to score before your claim is accepted: claim first.\n\
- An accepted claim is final: claim only what you will build, never a placeholder, a probe or a test.\n\
- Build and fix; do not optimize: no parameter sweeps, no tuning for score, no switching ideas. Later rounds refine the \
strongest candidates.\n- Every state you score must keep the program's rules and limits (size, time, determinism): check them before each \
mcp.ar.eval. An audit discards a state that breaks one.\n";
        let checklist = "Before your final answer, check: [ ] my claim was accepted [ ] my last state was scored by mcp.ar.eval and \
is valid [ ] my final answer says what I built, my best score, what worked and what did not.\n";
        Ok(match plan.role {
            Role::Research => format!(
                "{context}{}## Autoresearch research lead ({label})\n\n\
You lead an autoresearch run. Next, {WORKERS} workers will each pursue a different idea to improve the score by changing the \
editable paths ({}). Study the task so their work is grounded, then write the brief every worker gets:\n\
1. The objective, and exactly how the evaluator computes the score (read the code and data you can see).\n\
2. Rules and constraints: what is banned, what makes a result invalid.\n\
3. What the starting state does and where it is weak.\n\
4. Traps: ways a score could rise without real improvement (evaluator quirks, overfitting, degenerate solutions).\n\
5. Open questions worth testing.\n\n<rules>\n- Do not solve the task: the workers do that.\n\
- Keep the study to about 15 steps: an unfinished study leaves every worker without a brief.\n\
- Your final answer is the brief: at most 800 words, facts first, without prescribing one solution.\n</rules>\n",
                self.tools(plan.role),
                spec.editable.join(", ")
            ),
            Role::Explore => {
                // Only earlier rounds' claims: a retried worker keeps its own claim (claim gives it back) and builds on it.
                let round = label.split('-').next();
                let taken: String = state
                    .claims
                    .iter()
                    .filter(|c| c.scope == "explore" && c.worker.split('-').next() != round)
                    .map(|c| format!("- #{}: {}\n", c.n, c.idea))
                    .collect();
                let taken = if taken.is_empty() {
                    String::new()
                } else {
                    format!(
                        " Earlier rounds already claimed these ideas; yours must use a different core approach:\n{}",
                        clip(&taken, BRIEF_CHARS)
                    )
                };
                format!(
                    "{context}{}## Autoresearch explore worker {label}\n\n\
You are one of {WORKERS} workers exploring side by side; each builds a different core approach to improving the score.\n\n\
1. Generate your idea from your seed: <seed>{}</seed>. List 5 to 7 root-level dimensions along which approaches to this \
task differ fundamentally (not parameters or filters), each with at least 6 options from conventional to radical. Pick one \
option per dimension using a different part of the seed (for example the sum of the character codes of a segment, mod the \
number of options). Commit to the picks even if unusual, and compose one coherent idea: a core approach, not a tweak on \
top of the starting state.{taken}\n\
2. Read `mcp.ar.claims{{}}`, then claim your idea within your first few steps: \
`mcp.ar.claim{{idea = \"the idea in two or three sentences\"}}`. If it returns `ok = false`, the same core is taken: pick \
again from a different part of your seed and claim another.\n\
3. Build the simplest version of the idea that runs, and score it with mcp.ar.eval within about 10 steps of claiming. Then \
read the report and fix what is broken until the idea works as designed. Your first valid state sets your bar, even \
below the starting state's score.\n\
4. Your final answer is your finding: the idea, what you built, your best score, what worked and what did not, and \
whether the idea is promising.\n\n<rules>\n{worker_rules}</rules>\n\n{checklist}",
                    self.tools(plan.role),
                    plan.seed.as_deref().unwrap_or(label)
                )
            }
            Role::Exploit => {
                let rows: Vec<&Row> = state.rows.iter().filter(|r| r.idea == plan.idea).collect();
                let skip = rows.len().saturating_sub(SHOWN);
                let mut ledger = format!(
                    "<ledger>\nn | score | improved | worker | note\n{}",
                    if skip > 0 {
                        format!("({skip} older rows omitted)\n")
                    } else {
                        String::new()
                    }
                );
                for r in &rows[skip..] {
                    let score = match (r.valid, r.score) {
                        (true, Some(s)) => format!("{s:.6}"),
                        _ => "invalid".into(),
                    };
                    ledger += &format!(
                        "{} | {score} | {} | {} | {}\n",
                        r.n,
                        if r.improved { "yes" } else { "" },
                        r.episode,
                        r.note.replace('\n', " ")
                    );
                }
                format!(
                    "{context}{ledger}</ledger>\n\n{}## Autoresearch exploit worker {label}: candidate #{}\n\n\
Candidate #{}'s idea: {}\n\nIt is among the most promising candidates. Several workers refine it side by side, each with \
a different refinement.\n\n\
1. Read the ledger and the findings, then choose one refinement: a change with a reason to generalize.\n\
2. Read `mcp.ar.claims{{}}`, then claim it within your first few steps: \
`mcp.ar.claim{{idea = \"the refinement in one or two sentences\"}}`. If it returns `ok = false`, the same refinement is \
taken: choose another.\n\
3. Implement it and score it with mcp.ar.eval within about 10 steps of claiming; read the report and fix it if it is broken. \
It improves the candidate when it beats its best ({}).\n\
4. Your final answer is your finding: what you changed, your best score, what worked and what did not.\n\n\
<rules>\n{worker_rules}- One refinement: return once it is built and scored; the next round builds on the best result.\n\
</rules>\n\n{checklist}",
                    self.tools(plan.role),
                    plan.idea,
                    plan.idea,
                    claim(plan.idea),
                    show(plan.best)
                )
            }
            Role::Audit => format!(
                "{context}{}## Autoresearch auditor {label}: candidate #{}\n\n\
Candidate #{} ({}) is about to be kept as a result. Your workspace holds its best state. Decide whether its score is earned:\n\
1. Read the state and explain where its score comes from.\n\
2. Look for an evaluator hole: outcomes the real system would not give, lookahead or leaked information, data the \
solution should not see, hard-coded answers, rule violations, or degenerate behaviour the score rewards by mistake.\n\
3. To test a suspicion, write probe states (for example the trick in isolation) and score them with mcp.ar.eval.\n\n\
<rules>\n- Overfitting alone is not a failure: held-out data handles that.\n\
- Your final answer ends with exactly one line: `VERDICT: PASS` (the score is earned) or `VERDICT: FAIL: reason` (it \
comes from a hole or a rule violation). Without that line the audit counts for nothing.\n</rules>\n",
                self.tools(plan.role),
                plan.idea,
                plan.idea,
                claim(plan.idea)
            ),
        })
    }

    /// Every candidate (an accepted explore claim with a valid improved state, at its best search state), and the ones
    /// that may go on, strongest first. A state's search score is the mean of its evaluations (see `Estimate`). Good:
    /// beats the seed on search by more than `min_delta` and more than their combined standard error, and is not worse
    /// than the seed on select by more than theirs (a veto, never a ranking). Ranked good ones first, then by search:
    /// workers return their ideas unpolished, so one below the seed still earns refinement. A state that failed an
    /// audit is never a candidate's best; the seed's own state, or a state another candidate ranked higher already
    /// holds, never goes on.
    async fn candidates(&self) -> Result<(Vec<Candidate>, Vec<u32>)> {
        let seed = self.seed()?;
        let state = self.state()?;
        let records = self.records()?;
        let spec = self.spec();
        let seed_search = estimate(&state.records, &seed);
        let seed_select = self.select(&seed).await?;
        let mut candidates = vec![];
        let verdict = |idea: u32, at: &str| {
            records.iter().rev().find_map(|r| match r {
                Record::Audited {
                    idea: i,
                    snap,
                    pass,
                    reason,
                } if *i == idea && snap == at => Some((*pass, reason.clone())),
                _ => None,
            })
        };
        for c in state.claims.iter().filter(|c| c.scope == "explore") {
            // Its best state among those its workers improved it with; a state an audit failed costs only itself.
            let mut best: Option<(&str, Estimate)> = None;
            for r in state
                .rows
                .iter()
                .filter(|r| r.idea == c.n && r.improved && r.valid)
            {
                if best.is_some_and(|(snap, _)| snap == r.snap)
                    || verdict(c.n, &r.snap).is_some_and(|(pass, _)| !pass)
                {
                    continue;
                }
                let Some(e) = estimate(&state.records, &r.snap) else {
                    continue;
                };
                if best.is_none_or(|(_, b)| spec.better(e.mean, Some(b.mean), 0.0)) {
                    best = Some((&r.snap, e));
                }
            }
            let Some((snap, search)) = best else { continue };
            let beats = spec.beats(search, seed_search, spec.min_delta);
            // The veto is computed only where it can decide something, so held-out data is looked at as little as possible.
            let select = if beats {
                self.select(snap).await?
            } else {
                None
            };
            let held = spec.select.is_none()
                || select.is_some_and(|s| seed_select.is_none_or(|b| !spec.beats(b, Some(s), 0.0)));
            let audit = verdict(c.n, snap);
            candidates.push(Candidate {
                idea: c.n,
                claim: c.idea.clone(),
                snap: snap.to_string(),
                search: Some(search.mean),
                se: search.se,
                select: select.map(|s| s.mean),
                good: beats && held,
                audit: audit.as_ref().map(|a| a.0),
                reason: audit.map(|a| a.1).unwrap_or_default(),
            });
        }
        let mut ranked: Vec<&Candidate> = candidates.iter().filter(|c| c.snap != seed).collect();
        ranked.sort_by(|a, b| {
            b.good.cmp(&a.good).then_with(|| {
                let (a, b) = (a.search.unwrap_or(f64::NAN), b.search.unwrap_or(f64::NAN));
                if spec.better(a, Some(b), 0.0) {
                    std::cmp::Ordering::Less
                } else if spec.better(b, Some(a), 0.0) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
        });
        let mut seen = std::collections::HashSet::new();
        ranked.retain(|c| seen.insert(c.snap.clone()));
        let ranked = ranked.iter().map(|c| c.idea).collect();
        Ok((candidates, ranked))
    }

    /// The gate after `round`: audit the good candidates among the `TOP` strongest that have no verdict yet, in one
    /// batch, then hand the `TOP` strongest to the next round. A candidate whose state fails falls back to its next best
    /// state, or drops out; a candidate that takes another's place is audited at a later gate. Only audited good
    /// candidates become results.
    async fn gate(&mut self, round: usize) -> Result<Vec<u32>> {
        let batch = format!("audit{round}");
        // A resumed run finishes the batch it planned, so verdicts come from that batch's own plans.
        let plans = match self.planned(&batch)? {
            Some(plans) => plans,
            None => {
                let (candidates, ranked) = self.candidates().await?;
                let plans: Vec<Plan> = ranked
                    .iter()
                    .take(TOP)
                    .filter_map(|id| candidates.iter().find(|c| c.idea == *id))
                    .filter(|c| c.good && c.audit.is_none())
                    .map(|c| Plan {
                        label: format!("a{round}-c{}", c.idea),
                        role: Role::Audit,
                        idea: c.idea,
                        start: c.snap.clone(),
                        best: None,
                        seed: None,
                    })
                    .collect();
                if !plans.is_empty() {
                    self.plan(&batch, plans.clone())?;
                }
                plans
            }
        };
        if !plans.is_empty() {
            self.run_batch(&batch).await?;
            self.judge_audits(&plans)?;
        }
        let (candidates, ranked) = self.candidates().await?;
        let chosen: Vec<u32> = ranked.into_iter().take(TOP).collect();
        eprintln!(
            "hx-ar: gate {round}: {} candidates, {} good, going on with {:?}",
            candidates.len(),
            candidates.iter().filter(|c| c.good).count(),
            chosen
        );
        self.journal(&Record::Gate {
            round,
            candidates,
            chosen: chosen.clone(),
        })?;
        Ok(chosen)
    }

    /// Journal the verdict of each audit plan that ran and has none yet, from its last attempt's final answer. An
    /// auditor that crashed, or that stopped without a `VERDICT: PASS` or `VERDICT: FAIL` line, gives none: its
    /// candidate stays unaudited, so it cannot become a result until a later gate audits it.
    fn judge_audits(&self, plans: &[Plan]) -> Result<()> {
        let records = self.records()?;
        let ran = |p: &Plan| {
            records.iter().any(|r| matches!(r, Record::Started { label, plan, .. } if *plan == p.label
            && records.iter().any(|f| matches!(f, Record::Finished { label: l, crashed: false, .. } if l == label))))
        };
        for plan in plans.iter().filter(|p| !audited(&records, p) && ran(p)) {
            let Some((pass, reason)) = verdict(self.answer(&records, &plan.label).as_deref())
            else {
                eprintln!(
                    "hx-ar: audit of #{}: no verdict; it stays unaudited",
                    plan.idea
                );
                continue;
            };
            eprintln!(
                "hx-ar: audit of #{}: {}{}",
                plan.idea,
                if pass { "pass" } else { "FAIL" },
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                }
            );
            self.journal(&Record::Audited {
                idea: plan.idea,
                snap: plan.start.clone(),
                pass,
                reason,
            })?;
        }
        Ok(())
    }

    /// Close the episodes a stopped orchestrator left open. A child whose report the orchestrator saved had ended:
    /// it finished as it would have. Any other was interrupted: a worker that scored nothing, research and audits
    /// (judged by a final answer a killed child never gave) are retried, without counting as a crash. The child's
    /// journals, left in hx's state directory, say how often it was rate-limited.
    fn close_open(&self, records: &mut Vec<Record>) -> Result<()> {
        let rows: Vec<Row> = rows(records).cloned().collect();
        let episodes = self.dir.join("episodes");
        let open: Vec<(String, Option<PathBuf>)> = records
            .iter()
            .filter_map(|r| match r {
                Record::Started { label, scratch, .. }
                    if !records
                        .iter()
                        .any(|f| matches!(f, Record::Finished { label: l, .. } if l == label)) =>
                {
                    Some((label.clone(), scratch.clone()))
                }
                _ => None,
            })
            .collect();
        for (label, scratch) in open {
            let evals = rows.iter().filter(|r| r.episode == label).count();
            let report = std::fs::read(episodes.join(format!("{label}.report.json")))
                .ok()
                .map(|b| serde_json::from_slice::<Value>(&b))
                .transpose()
                .context("episode report")?;
            let record = match report {
                Some(report) => Record::Finished {
                    crashed: evals == 0 && report.is_null(),
                    interrupted: false,
                    rate_limits: report["rate_limits"].as_u64().unwrap_or(0),
                    label,
                },
                None => {
                    let rate_limits = match scratch {
                        Some(scratch) => rate_limited(&keep_logs(
                            &self.dirs.workspace(&scratch.join("ws")).log,
                            &episodes,
                            &label,
                        )?),
                        None => 0,
                    };
                    let answer_only = label.starts_with("r0-") || label.starts_with('a');
                    Record::Finished {
                        crashed: evals == 0 || answer_only,
                        interrupted: true,
                        rate_limits,
                        label,
                    }
                }
            };
            self.journal(&record)?;
            records.push(record);
        }
        Ok(())
    }

    async fn run(&mut self, mut records: Vec<Record>) -> Result<Value> {
        self.close_open(&mut records)?;
        // Scratch copies of processes that stopped without removing them, this run's included, once their journals are kept.
        tokio::task::spawn_blocking(sweep).await?;
        if baseline(&self.records()?).is_none() {
            self.baseline().await?;
        }
        let seed = self.seed()?;
        // A select evaluator that cannot score the seed would reject every candidate at the first gate: stop before spending.
        if self.spec().select.is_some() {
            ensure!(
                self.select(&seed).await?.is_some(),
                "the select evaluator gives no valid score for the seed; fix it before searching"
            );
        }
        self.plan(
            "research",
            vec![Plan {
                label: "r0-research".into(),
                role: Role::Research,
                idea: 0,
                start: seed.clone(),
                best: None,
                seed: None,
            }],
        )?;
        self.run_batch("research").await?;
        // Round 1 explores. A round makes progress as `progress` says, so small steps add up across rounds until they
        // count. After exploring the run refines; it refines again while that moves a chosen candidate at all, and
        // explores again after a refining round that moved none. Two rounds in a row without progress stop the run, so
        // it ends once the search stops finding changes the evaluator can tell apart from its noise.
        let (mut at, mut prev) = (
            Vec::<(u32, Option<Estimate>)>::new(),
            Vec::<(u32, String)>::new(),
        );
        let (mut explore, mut quiet) = (true, false);
        for round in 1.. {
            let batch = format!("round{round}");
            let mut gate = self.records()?.into_iter().find_map(|r| match r {
                Record::Gate {
                    round: g,
                    candidates,
                    chosen,
                } if g == round => Some((candidates, chosen)),
                _ => None,
            });
            let plans = match self.planned(&batch)? {
                Some(plans) => plans,
                None if gate.is_some() => vec![],
                None => {
                    let workers = self.width()?;
                    let plans = if explore {
                        (1..=workers)
                            .map(|w| -> Result<Plan> {
                                // Fresh OS entropy per worker, for String Seed of Thought (https://pub.sakana.ai/ssot/):
                                // nothing earlier in the run can predict it, so workers' ideas move off the model's default.
                                Ok(Plan {
                                    label: format!("r{round}-w{w}"),
                                    role: Role::Explore,
                                    idea: 0,
                                    start: seed.clone(),
                                    best: None,
                                    seed: Some(hx::tools::nonce()?),
                                })
                            })
                            .collect::<Result<Vec<_>>>()?
                    } else {
                        let (candidates, chosen) = self.last_gate()?.unwrap_or_default();
                        (0..workers)
                            .map(|w| {
                                let c = candidates
                                    .iter()
                                    .find(|c| c.idea == chosen[w % chosen.len()])
                                    .expect("chosen candidates are listed");
                                Plan {
                                    label: format!("r{round}-w{}", w + 1),
                                    role: Role::Exploit,
                                    idea: c.idea,
                                    start: c.snap.clone(),
                                    best: c.search,
                                    seed: None,
                                }
                            })
                            .collect()
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
            if chosen.is_empty() {
                break;
            }
            let chosen: Vec<&Candidate> = chosen
                .iter()
                .filter_map(|id| candidates.iter().find(|c| c.idea == *id))
                .collect();
            let progress = progress(self.spec(), &chosen, &at);
            let moved = chosen
                .iter()
                .any(|c| !prev.contains(&(c.idea, c.snap.clone())));
            if !progress && quiet {
                eprintln!("hx-ar: two rounds in a row made no progress; stopping");
                break;
            }
            quiet = !progress;
            explore = !moved && plans.first().is_some_and(|p| p.role == Role::Exploit);
            if progress {
                at = chosen.iter().map(|c| (c.idea, c.estimate())).collect();
            }
            prev = chosen.iter().map(|c| (c.idea, c.snap.clone())).collect();
        }
        let state = self.state()?;
        ensure!(
            !state.rows.is_empty(),
            "autoresearch made no evaluations; see {}",
            self.dir.join("episodes").display()
        );
        let (candidates, chosen) = self.last_gate()?.unwrap_or_default();
        let portfolio: Vec<&Candidate> = chosen
            .iter()
            .filter_map(|id| candidates.iter().find(|c| c.idea == *id))
            .filter(|c| c.good && c.audit == Some(true))
            .collect();
        // The portfolio's states, best search score first; the first goes into the workspace (the seed when none).
        for c in &portfolio {
            let dir = self.dir.join("portfolio").join(format!("c{}", c.idea));
            if !dir.exists() {
                place(&dir, self.spec(), &load_snap(&self.dir, &c.snap)?)?;
            }
        }
        let (snap, search, select) = match portfolio.first() {
            Some(c) => (c.snap.clone(), c.search, c.select),
            None => (
                seed.clone(),
                self.known(&state.records, &seed).0.map(|e| e.mean),
                self.select(&seed).await?.map(|e| e.mean),
            ),
        };
        if snap == seed {
            eprintln!("hx-ar: no candidate beat the seed; the workspace keeps the seed");
        } else {
            apply(
                &self.workspace,
                self.spec(),
                &load_snap(&self.dir, &seed)?,
                &load_snap(&self.dir, &snap)?,
            )?;
        }
        let report = json!({
            "run": self.dir.file_name().map(|n| n.to_string_lossy()), "dir": self.dir, "commit": self.launch.commit,
            "snap": snap, "search": search, "select": select, "portfolio": portfolio, "candidates": candidates,
            "rounds": self.records()?.iter().filter(|r| matches!(r, Record::Gate { .. })).count(),
            "evals": state.rows.len(),
            "judge": judge_totals(&state.records),
        });
        std::fs::write(
            self.dir.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        Ok(report)
    }

    /// The last gate's candidates and the ideas it chose.
    fn last_gate(&self) -> Result<Option<(Vec<Candidate>, Vec<u32>)>> {
        Ok(self.records()?.into_iter().rev().find_map(|r| match r {
            Record::Gate {
                candidates, chosen, ..
            } => Some((candidates, chosen)),
            _ => None,
        }))
    }
}

/// How the attempts of one planned episode went so far.
#[derive(Debug, Default, PartialEq)]
struct Attempts {
    /// Attempts started, which numbers the next one.
    started: usize,
    /// One finished without crashing: the episode is done.
    succeeded: bool,
    /// Attempts that crashed since the batch last stalled. An interrupted attempt is no crash: the orchestrator, not
    /// the episode, stopped it.
    crashes: u32,
}

/// The attempts of the planned episode `plan` of `batch`, from the journal.
fn attempts(records: &[Record], batch: &str, plan: &str) -> Attempts {
    let since = records
        .iter()
        .rposition(|r| matches!(r, Record::Stalled { batch: b } if b == batch))
        .map_or(0, |i| i + 1);
    let mut out = Attempts::default();
    for (i, r) in records.iter().enumerate() {
        let Record::Started { label, plan: p, .. } = r else {
            continue;
        };
        if p != plan {
            continue;
        }
        out.started += 1;
        let finished = records.iter().find_map(|f| match f {
            Record::Finished {
                label: l,
                crashed,
                interrupted,
                ..
            } if l == label => Some((*crashed, *interrupted)),
            _ => None,
        });
        match finished {
            Some((false, _)) => out.succeeded = true,
            Some((true, false)) if i >= since => out.crashes += 1,
            _ => {}
        }
    }
    out
}

/// Has this audit plan's verdict been journaled?
fn audited(records: &[Record], plan: &Plan) -> bool {
    records.iter().any(|r| matches!(r, Record::Audited { idea, snap, .. } if *idea == plan.idea && *snap == plan.start))
}

/// What the journal says, as workers are shown it.
struct State {
    records: Vec<Record>,
    rows: Vec<Row>,
    claims: Vec<Claim>,
    brief: String,
    findings: String,
}

/// An auditor's verdict from its last `VERDICT:` line, markdown emphasis and case aside: pass on `PASS`, fail with its
/// reason on `FAIL`. None when it gave no such line, or a verdict that is neither.
fn verdict(answer: Option<&str>) -> Option<(bool, String)> {
    let answer = answer?;
    let emphasis = |c: char| matches!(c, '*' | '_' | '`') || c.is_whitespace();
    // ASCII upper-casing keeps byte offsets, so a match in `upper` is the same place in `answer`.
    let upper = answer.to_ascii_uppercase();
    let rest = upper
        .rmatch_indices("VERDICT")
        .find_map(|(at, _)| {
            answer[at + "VERDICT".len()..]
                .trim_start_matches(emphasis)
                .strip_prefix(':')
        })?
        .trim_start_matches(emphasis);
    let word: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
    match word.to_ascii_uppercase().as_str() {
        "PASS" => Some((true, String::new())),
        "FAIL" => {
            let reason = rest[word.len()..]
                .trim_start_matches(|c: char| emphasis(c) || matches!(c, ':' | '-' | '.'));
            Some((
                false,
                reason
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(emphasis)
                    .chars()
                    .take(300)
                    .collect(),
            ))
        }
        _ => None,
    }
}

/// At most `max` characters of `text`, marked when cut.
fn clip(text: &str, max: usize) -> String {
    let cut: String = text.chars().take(max).collect();
    if cut.len() < text.len() {
        cut + "\n... (truncated)"
    } else {
        cut
    }
}

fn show(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.6}"))
        .unwrap_or_else(|| "none".into())
}

/// The workspace's autoresearch directory, which holds its runs: `ar/` in its state directory.
pub(crate) fn runs_dir(dirs: &Dirs, workspace: &Path) -> PathBuf {
    dirs.workspace(workspace).state.join("ar")
}

/// Start a run: freeze the committed tree and the seed, then search until two rounds in a row make no progress.
/// `child_args` go to every episode's `hx run`; `program` is the task text.
pub(crate) async fn start(
    workspace: &Path,
    dirs: &Dirs,
    child_args: Vec<String>,
    program: String,
) -> Result<Value> {
    let config = hx::trust::trusted(workspace, dirs)?.context(
        "hx-ar runs the evaluator commands in .harness/ar.toml; review them and run `hx trust` first",
    )?;
    ensure!(
        hx::workflow::git_inventory(workspace)?.is_some(),
        "hx-ar requires a Git repository at the workspace root"
    );
    let spec = Spec::load(workspace, &config, dirs)?;
    let bad = uncommitted(workspace, &spec)?;
    ensure!(
        bad.is_empty(),
        "hx-ar freezes the committed tree: commit or remove changes outside the editable paths: {}",
        bad.join(", ")
    );
    let id = format!("{}-{}", chrono_stamp(), std::process::id());
    let dir = runs_dir(dirs, workspace).join(&id);
    std::fs::create_dir_all(&dir)?;
    let commit = freeze(workspace, &dir)?;
    let launch = Launch {
        spec,
        program,
        child_args,
        commit,
    };
    std::fs::write(dir.join("launch.json"), serde_json::to_vec_pretty(&launch)?)?;
    let journal = Journal::open(&dir)?;
    journal.append(Record::Seed {
        snap: save_snap(&dir, &capture(workspace, &launch.spec)?)?,
    })?;
    orchestrate(workspace, dirs, dir, launch, journal).await
}

/// Continue the run in `dir` (a run of this workspace) with the spec and flags frozen in its `launch.json`, approved
/// when it started; only a launch reads `ar.toml`.
pub(crate) async fn resume(workspace: &Path, dirs: &Dirs, dir: &Path) -> Result<Value> {
    let runs = runs_dir(dirs, workspace);
    let dir = dir
        .canonicalize()
        .with_context(|| format!("autoresearch run {}", dir.display()))?;
    ensure!(
        dir.parent() == Some(runs.as_path()),
        "--resume takes a run of this workspace, in {}",
        runs.display()
    );
    ensure!(
        !dir.join("report.json").exists(),
        "autoresearch run {} already finished",
        dir.display()
    );
    ensure!(
        !dir.join("ledger.jsonl").exists(),
        "autoresearch run {} predates this version of hx-ar and cannot be resumed; start a new run",
        dir.display()
    );
    let launch: Launch =
        serde_json::from_slice(&std::fs::read(dir.join("launch.json"))?).context("launch.json")?;
    require_confinement(&launch.child_args)?;
    let journal = Journal::open(&dir)?;
    ensure!(
        seed(&journal.records()).is_some(),
        "autoresearch run {} stopped before it recorded its seed; start a new run",
        dir.display()
    );
    orchestrate(workspace, dirs, dir, launch, journal).await
}

async fn orchestrate(
    workspace: &Path,
    dirs: &Dirs,
    dir: PathBuf,
    launch: Launch,
    journal: Journal,
) -> Result<Value> {
    let records = journal.records();
    let judge = crate::serve::Judge::new(&launch.child_args);
    let shared = Arc::new(Shared::new(
        dir.clone(),
        launch.spec.clone(),
        journal,
        judge,
    ));
    shared.listen().await?;
    let mut o = Orchestrator {
        dirs: dirs.clone(),
        workspace: workspace.to_path_buf(),
        dir,
        launch,
        shared,
    };
    eprintln!(
        "hx-ar: run {}{}",
        o.dir.display(),
        if records.iter().any(|r| matches!(r, Record::Planned { .. })) {
            ", resumed"
        } else {
            ""
        }
    );
    o.run(records).await
}

/// The model settings in an episode's `hx run` flags (`--model`, else `HX_MODEL`, and so on), for the claim judge and
/// for the provider's limits; None without a model.
pub(crate) fn llm_cfg(child_args: &[String]) -> Option<hx::llm::LlmCfg> {
    let flag = |name: &str| {
        child_args
            .iter()
            .position(|a| a == name)
            .and_then(|i| child_args.get(i + 1))
            .cloned()
    };
    Some(hx::llm::LlmCfg {
        model: flag("--model").or_else(|| std::env::var("HX_MODEL").ok())?,
        base_url: flag("--base-url").or_else(|| std::env::var("HX_BASE_URL").ok()),
        key_env: flag("--key-env"),
        reasoning: None,
        max_output: JUDGE_TOKENS,
        context_window: None,
        cache_key: None,
        provider_only: flag("--provider-only")
            .map(|v| v.split(',').map(String::from).collect())
            .unwrap_or_default(),
    })
}

/// Output tokens a judge call may use.
pub(crate) const JUDGE_TOKENS: u64 = 2000;

/// The `hx` binary episodes run: the one installed beside this program.
fn hx_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate hx-ar")?;
    let hx = exe.with_file_name("hx");
    ensure!(
        hx.is_file(),
        "hx-ar runs its episodes with the hx binary beside it, and there is none at {}",
        hx.display()
    );
    Ok(hx)
}

/// How long an episode's `hx` waits for one call to this process's MCP server: long enough for an evaluation that
/// waits on another of the same state and then runs to the evaluator's timeout, or for a claim waiting its turn
/// behind the judge calls of other claims.
fn call_deadline(spec: &Spec) -> u64 {
    (2.0 * spec.timeout).ceil() as u64 + 600
}

/// Give the episode workspace `ws` the configuration its child runs with: `.harness/mcp.toml` names this process's
/// server, `url`, with the episode's token from `TOKEN_ENV`, and `.harness/policy.toml` pre-approves its tools and
/// seals the evaluators' roots from the agent's file tools in every mode.
fn configure(ws: &Path, spec: &Spec, url: &str) -> Result<()> {
    let harness = ws.join(".harness");
    std::fs::create_dir_all(&harness)?;
    let mcp = json!({"server": {"ar": {
        "url": url,
        "bearer_env": TOKEN_ENV,
        "read_only_tools": ["claims"],
        "timeout_secs": call_deadline(spec),
    }}});
    let sealed: Vec<String> = spec
        .roots(true)?
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect();
    let policy = json!({"approval": {"allow": ["mcp ar.*"]}, "fs": {"sealed": sealed}});
    std::fs::write(harness.join("mcp.toml"), toml::to_string(&mcp)?)?;
    std::fs::write(harness.join("policy.toml"), toml::to_string(&policy)?)?;
    Ok(())
}

/// Make the child of `command` stop when this process dies, however it dies: the kernel sends it SIGTERM
/// (`PR_SET_PDEATHSIG`), and hx stops its own commands on that signal. The signal is tied to the thread that spawns the
/// child: the run loop spawns children from the thread that runs `main`, which lives as long as the process.
fn stop_with_this_process(command: &mut tokio::process::Command) {
    let parent = std::process::id();
    // SAFETY: the closure runs in the forked child before exec, where only async-signal-safe calls are allowed: it
    // calls prctl(2) and getppid(2) and builds an `io::Error` from an errno, which does not allocate.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // This process may have died before the signal was asked for: then nothing will send it.
            if libc::getppid() as u32 != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// Refuse to start (or resume) a run whose evaluators and episodes could not be confined, unless the owner passed
/// `--unconfined` (`child_args` are the flags every episode gets, recorded in `launch.json`); with it, say once that
/// they run unconfined. The episodes' `hx run` gets the same flag.
pub fn require_confinement(child_args: &[String]) -> Result<()> {
    if child_args.iter().any(|a| a == "--unconfined") {
        hx::confine::allow_unconfined();
    }
    hx::confine::require()?;
    hx::confine::note();
    Ok(())
}

fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_is_max_or_min_and_stored_as_before() {
        let spec = |extra: &str| {
            toml::from_str::<Spec>(&format!("editable = [\"x\"]\neval = \"true\"\n{extra}"))
        };
        assert_eq!(spec("").unwrap().direction, Direction::Max);
        assert_eq!(
            spec("direction = \"min\"").unwrap().direction,
            Direction::Min
        );
        assert!(
            spec("direction = \"lowest\"").is_err(),
            "an unknown direction is refused, not read as min"
        );
        let stored = serde_json::to_value(spec("direction = \"min\"").unwrap()).unwrap();
        assert_eq!(stored["direction"], "min");
        let mut s = spec("direction = \"min\"").unwrap();
        assert!(s.better(1.0, Some(2.0), 0.0) && !s.better(3.0, Some(2.0), 0.0));
        s.direction = Direction::Max;
        assert!(s.better(3.0, Some(2.0), 0.0));
    }

    #[test]
    fn audit_verdicts() {
        assert_eq!(
            verdict(Some("ok\nVERDICT: PASS")),
            Some((true, String::new()))
        );
        assert_eq!(
            verdict(Some("verdict: fail: fills below the market")),
            Some((false, "fills below the market".into()))
        );
        assert_eq!(
            verdict(Some("**VERDICT:** PASS")),
            Some((true, String::new()))
        );
        assert_eq!(
            verdict(Some("**Verdict**: Pass.")),
            Some((true, String::new()))
        );
        assert_eq!(
            verdict(Some("lookahead.\n\nVERDICT: **FAIL**: reads future bars")),
            Some((false, "reads future bars".into()))
        );
        assert_eq!(
            verdict(Some("VERDICT: `FAIL` - leaks the answer")),
            Some((false, "leaks the answer".into()))
        );
        assert_eq!(
            verdict(Some(
                "VERDICT: PASS\nOn reflection, VERDICT: FAIL: hard-coded"
            )),
            Some((false, "hard-coded".into()))
        );
        assert_eq!(
            verdict(Some("VERDICT: FAIL: x\nthe verdict above stands")),
            Some((false, "x".into()))
        );
        assert_eq!(verdict(Some("VERDICT: UNSURE")), None);
        assert_eq!(verdict(Some("VERDICT: PASSABLE")), None);
        assert_eq!(verdict(Some("VERDICT: probably pass")), None);
        assert_eq!(verdict(None), None);
        assert_eq!(verdict(Some("ran out")), None);
    }

    fn started(label: &str, plan: &str) -> Record {
        Record::Started {
            label: label.into(),
            plan: plan.into(),
            scratch: None,
        }
    }
    fn finished(label: &str, crashed: bool, interrupted: bool) -> Record {
        Record::Finished {
            label: label.into(),
            interrupted,
            crashed,
            rate_limits: 0,
        }
    }

    #[test]
    fn interruptions_and_stalls_do_not_use_up_crash_retries() {
        let mut records = vec![
            started("r1-w1", "r1-w1"),
            finished("r1-w1", true, true),
            started("r1-w1.2", "r1-w1"),
            finished("r1-w1.2", true, false),
        ];
        assert_eq!(
            attempts(&records, "round1", "r1-w1"),
            Attempts {
                started: 2,
                succeeded: false,
                crashes: 1
            }
        );
        records.extend([
            started("r1-w1.3", "r1-w1"),
            finished("r1-w1.3", true, false),
            started("r1-w1.4", "r1-w1"),
            finished("r1-w1.4", true, false),
        ]);
        assert_eq!(attempts(&records, "round1", "r1-w1").crashes, MAX_CRASHES);
        // A stall of another batch changes nothing; a stall of this one gives fresh attempts.
        records.push(Record::Stalled {
            batch: "round2".into(),
        });
        assert_eq!(attempts(&records, "round1", "r1-w1").crashes, MAX_CRASHES);
        records.push(Record::Stalled {
            batch: "round1".into(),
        });
        assert_eq!(
            attempts(&records, "round1", "r1-w1"),
            Attempts {
                started: 4,
                succeeded: false,
                crashes: 0
            }
        );
        records.extend([
            started("r1-w1.5", "r1-w1"),
            finished("r1-w1.5", false, false),
        ]);
        assert!(attempts(&records, "round1", "r1-w1").succeeded);
    }

    #[test]
    fn re_scoring_unchanged_files_is_not_progress() {
        let spec: Spec = toml::from_str("editable = [\"x\"]\neval = \"true\"\n").unwrap();
        let candidate = |mean: f64, se: Option<f64>| Candidate {
            idea: 1,
            claim: String::new(),
            snap: "s".into(),
            search: Some(mean),
            se,
            select: None,
            good: true,
            audit: None,
            reason: String::new(),
        };
        let at = [(
            1,
            Some(Estimate {
                mean: 1.0,
                se: Some(0.5),
            }),
        )];
        // A second evaluation of the same state moved the mean by 0.3 and shrank its se to 0.35: well within noise.
        assert!(!progress(&spec, &[&candidate(1.3, Some(0.35))], &at));
        assert!(progress(&spec, &[&candidate(2.0, Some(0.35))], &at));
        // A new candidate is progress; a deterministic one, any gain above min_delta.
        assert!(progress(
            &spec,
            &[&Candidate {
                idea: 2,
                ..candidate(0.0, None)
            }],
            &at
        ));
        let exact = [(
            1,
            Some(Estimate {
                mean: 1.0,
                se: None,
            }),
        )];
        assert!(progress(&spec, &[&candidate(1.01, None)], &exact));
        assert!(!progress(&spec, &[&candidate(1.0, None)], &exact));
    }

    #[test]
    fn estimates_average_repeated_evaluations() {
        let spec: Spec =
            toml::from_str("editable = [\"x\"]\neval = \"true\"\nmin_delta = 0.1\n").unwrap();
        // Deterministic: exact, compared on min_delta alone.
        let exact = Estimate::of([(1.0, None), (1.0, None)]).unwrap();
        assert_eq!(
            exact,
            Estimate {
                mean: 1.0,
                se: None
            }
        );
        assert!(spec.beats(exact, Estimate::of([(0.85, None)]), 0.1));
        assert!(!spec.beats(exact, Estimate::of([(0.95, None)]), 0.1));
        // Noisy: the mean of the samples, its standard error shrinking with their number.
        let noisy = Estimate::of([
            (1.4, Some(0.5)),
            (0.6, Some(0.5)),
            (1.0, Some(0.5)),
            (1.0, Some(0.5)),
        ])
        .unwrap();
        assert!(
            (noisy.mean - 1.0).abs() < 1e-12 && (noisy.se.unwrap() - 0.25).abs() < 1e-12,
            "{noisy:?}"
        );
        let seed = Estimate {
            mean: 0.0,
            se: Some(0.5),
        };
        // Combined se is sqrt(0.25^2 + 0.5^2) ~ 0.56: 1.0 beats it, 0.5 does not, though 0.5 beats min_delta.
        assert!(spec.beats(noisy, Some(seed), 0.1));
        assert!(!spec.beats(Estimate { mean: 0.5, ..noisy }, Some(seed), 0.1));
        assert!(spec.beats(noisy, None, 0.1));
        assert_eq!(Estimate::of([]), None);
    }

    /// A run directory with the given seed, candidates' evaluations and journal, for the gate's ranking.
    struct Fixture {
        o: Orchestrator,
    }
    impl Fixture {
        fn new(
            tag: &str,
            rows: &[(u32, &str, f64, Option<f64>, bool)],
            records: &[Record],
        ) -> Fixture {
            let dir = std::env::temp_dir().join(format!("hx-artest-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let spec: Spec = toml::from_str("editable = [\"x\"]\neval = \"true\"\n").unwrap();
            let journal = Journal::open(&dir).unwrap();
            journal
                .append(Record::Seed {
                    snap: "seed".into(),
                })
                .unwrap();
            journal
                .append(Record::Baseline(Baseline {
                    score: Some(0.0),
                    se: None,
                    report: Value::Null,
                }))
                .unwrap();
            let ideas: std::collections::BTreeSet<u32> = rows.iter().map(|r| r.0).collect();
            for idea in ideas {
                journal
                    .append(Record::Claimed(Claim {
                        scope: "explore".into(),
                        n: idea,
                        idea: format!("idea {idea}"),
                        worker: format!("r1-w{idea}"),
                    }))
                    .unwrap();
            }
            for (i, (idea, snap, score, se, improved)) in rows.iter().enumerate() {
                journal
                    .append(Record::Evaluated(Row {
                        n: i + 1,
                        episode: format!("r1-w{idea}"),
                        idea: *idea,
                        note: String::new(),
                        snap: snap.to_string(),
                        score: Some(*score),
                        valid: true,
                        se: *se,
                        improved: *improved,
                        report: Value::Null,
                    }))
                    .unwrap();
            }
            for r in records {
                journal.append(r.clone()).unwrap();
            }
            let o = Orchestrator {
                dirs: Dirs::at(&dir.join("config"), &dir.join("state")),
                workspace: dir.clone(),
                dir: dir.clone(),
                launch: Launch {
                    spec: spec.clone(),
                    program: String::new(),
                    child_args: vec![],
                    commit: String::new(),
                },
                shared: Arc::new(Shared::new(dir.clone(), spec, journal, None)),
            };
            Fixture { o }
        }
        async fn gate(&self) -> (Vec<Candidate>, Vec<u32>) {
            self.o.candidates().await.unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.o.dir);
        }
    }

    fn audited(idea: u32, snap: &str, pass: bool) -> Record {
        Record::Audited {
            idea,
            snap: snap.into(),
            pass,
            reason: String::new(),
        }
    }

    #[tokio::test]
    async fn a_failed_audit_costs_only_the_state_it_failed() {
        // Candidate 1 improved to a, then b (failed), then c: c is its best now; a candidate never audited keeps its best.
        let rows = [
            (1, "a", 1.0, None, true),
            (1, "b", 2.0, None, true),
            (1, "c", 3.0, None, true),
            (2, "d", 1.5, None, true),
        ];
        let f = Fixture::new(
            "audit",
            &rows,
            &[audited(1, "a", true), audited(1, "b", false)],
        );
        let (candidates, ranked) = f.gate().await;
        let one = candidates.iter().find(|c| c.idea == 1).unwrap();
        assert_eq!(
            (one.snap.as_str(), one.search, one.audit),
            ("c", Some(3.0), None)
        );
        assert_eq!(ranked, vec![1, 2]);
        // Once c fails too, its best state that did not fail is a.
        let f = Fixture::new(
            "audit2",
            &rows,
            &[
                audited(1, "a", true),
                audited(1, "b", false),
                audited(1, "c", false),
            ],
        );
        let one = f.gate().await.0.into_iter().find(|c| c.idea == 1).unwrap();
        assert_eq!((one.snap.as_str(), one.audit), ("a", Some(true)));
    }

    #[tokio::test]
    async fn a_state_scores_the_mean_of_its_evaluations() {
        // Candidate 1's state was scored four times; one lucky sample does not make it the best, nor good.
        let rows = [
            (1, "a", 3.0, Some(1.2), true),
            (1, "a", -1.0, Some(1.2), false),
            (1, "a", 0.0, Some(1.2), false),
            (1, "a", 0.2, Some(1.2), false),
            (2, "b", 0.5, Some(0.1), true),
            (3, "c", 0.6, None, true),
        ];
        let f = Fixture::new("mean", &rows, &[]);
        let (candidates, ranked) = f.gate().await;
        let one = candidates.iter().find(|c| c.idea == 1).unwrap();
        // Mean 0.55 with se 1.2 * 2 / 4 = 0.6: not clear of the exact seed's 0.
        assert!(
            (one.search.unwrap() - 0.55).abs() < 1e-12
                && (one.se.unwrap() - 0.6).abs() < 1e-12
                && !one.good,
            "{one:?}"
        );
        // b beats the exact seed (0) by more than its se; c, exact, by anything.
        assert!(
            candidates.iter().filter(|c| c.idea != 1).all(|c| c.good),
            "{candidates:?}"
        );
        assert_eq!(ranked, vec![3, 2, 1]);
    }

    #[tokio::test]
    async fn only_the_seed_state_itself_is_dropped_at_the_gate() {
        // Candidate 1 ties the seed's score in other bytes, candidate 2 is below it, candidate 3 holds the seed's bytes.
        let rows = [
            (1, "a", 0.0, None, true),
            (2, "b", -1.0, None, true),
            (3, "seed", 0.0, None, true),
        ];
        let f = Fixture::new("tie", &rows, &[]);
        let (_, ranked) = f.gate().await;
        assert_eq!(ranked, vec![1, 2]);
    }

    #[tokio::test]
    async fn resume_closes_an_episode_whose_child_reported_as_finished() {
        let f = Fixture::new("close", &[], &[]);
        let episodes = f.o.dir.join("episodes");
        std::fs::create_dir_all(&episodes).unwrap();
        std::fs::write(
            episodes.join("r0-research.report.json"),
            r#"{"final_answer": "BRIEF", "rate_limits": 2}"#,
        )
        .unwrap();
        let mut records = vec![
            started("r0-research", "r0-research"),
            started("a1-c1", "a1-c1"),
        ];
        let before = f.o.records().unwrap().len();
        f.o.close_open(&mut records).unwrap();
        assert!(
            matches!(&records[2], Record::Finished { label, crashed: false, interrupted: false, rate_limits: 2 } if label == "r0-research")
        );
        assert!(
            matches!(&records[3], Record::Finished { label, crashed: true, interrupted: true, .. } if label == "a1-c1")
        );
        assert_eq!(f.o.records().unwrap().len(), before + 2);
    }

    #[test]
    fn holdout_roots_the_search_evaluator_could_read_are_refused() {
        let base = std::env::temp_dir().join(format!("hx-artest-overlap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dirs = Dirs::at(&base.join("config"), &base.join("state"));
        let (ws, grader) = (base.join("ws"), base.join("grader"));
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        std::fs::create_dir_all(grader.join("select")).unwrap();
        let load = |holdout: &Path| {
            std::fs::write(
                ws.join(".harness/ar.toml"),
                format!(
                    "editable = [\"x\"]\neval = \"true\"\ngrader = [\"{}\"]\n\
                holdout = [\"{}\"]\n",
                    grader.display(),
                    holdout.display()
                ),
            )
            .unwrap();
            Spec::load(&ws, &hx::trust::Config::read(&ws).unwrap(), &dirs)
                .map(drop)
                .map_err(|e| e.to_string())
        };
        assert!(
            load(&grader.join("select"))
                .unwrap_err()
                .contains("which the search evaluator may read")
        );
        assert!(
            load(Path::new("/etc"))
                .unwrap_err()
                .contains("which the agent's shell commands may read")
        );
        std::fs::create_dir_all(base.join("held")).unwrap();
        assert_eq!(load(&base.join("held")), Ok(()));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn grader_and_holdout_roots_may_not_overlap_hx_own_directories() {
        let base = std::env::temp_dir().join(format!("hx-artest-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (ws, home) = (base.join("home/ws"), base.join("home"));
        let dirs = Dirs::at(&home.join(".config/hx"), &home.join(".local/state/hx"));
        let inside_state = home.join(".local/state/hx/workspaces/other");
        for d in [
            ws.join(".harness"),
            dirs.config().to_path_buf(),
            inside_state.clone(),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        let load = |root: &str, key: &str| {
            std::fs::write(
                ws.join(".harness/ar.toml"),
                format!("editable = [\"x\"]\neval = \"true\"\n{key} = [\"{root}\"]\n"),
            )
            .unwrap();
            Spec::load(&ws, &hx::trust::Config::read(&ws).unwrap(), &dirs)
                .map(drop)
                .map_err(|e| e.to_string())
        };
        let home_root = home.join("data");
        std::fs::create_dir_all(&home_root).unwrap();
        // Home itself holds both directories; so does any root above them.
        for (root, key, what) in [
            (
                dirs.config().display().to_string(),
                "grader",
                "configuration",
            ),
            (inside_state.display().to_string(), "holdout", "state"),
            (home.join(".local").display().to_string(), "grader", "state"),
        ] {
            let error = load(&root, key).unwrap_err();
            assert!(
                error.contains(&format!("hx's {what} directory")),
                "{root}: {error}"
            );
        }
        assert_eq!(load(&home_root.display().to_string(), "grader"), Ok(()));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_grader_root_of_home_is_refused() {
        // `~` expands to HOME, which contains hx's own directories when they are in their default places.
        let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
        let dirs = Dirs::at(&home.join(".config/hx"), &home.join(".local/state/hx"));
        let ws = std::env::temp_dir().join(format!("hx-artest-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        std::fs::write(
            ws.join(".harness/ar.toml"),
            "editable = [\"x\"]\neval = \"true\"\ngrader = [\"~\"]\n",
        )
        .unwrap();
        let error = Spec::load(&ws, &hx::trust::Config::read(&ws).unwrap(), &dirs)
            .unwrap_err()
            .to_string();
        assert!(error.contains("overlaps hx's"), "{error}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn an_editable_path_may_not_reach_the_workspaces_skills() {
        let base = std::env::temp_dir().join(format!("hx-artest-skills-{}", std::process::id()));
        let dirs = Dirs::at(&base.join("config"), &base.join("state"));
        let ws = base.join("ws");
        for (editable, refused) in [
            (".agents", true),
            (".agents/skills", true),
            (".agents/skills/x", true),
            (".agents/artifacts", false),
        ] {
            let _ = std::fs::remove_dir_all(&ws);
            std::fs::create_dir_all(ws.join(".harness")).unwrap();
            std::fs::write(
                ws.join(".harness/ar.toml"),
                format!("editable = [\"{editable}\"]\neval = \"true\"\n"),
            )
            .unwrap();
            let loaded = Spec::load(&ws, &hx::trust::Config::read(&ws).unwrap(), &dirs);
            assert_eq!(
                loaded.is_err_and(|e| e.to_string().contains(".agents/skills")),
                refused,
                "{editable}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
