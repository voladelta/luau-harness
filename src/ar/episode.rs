//! Inside an episode: `ar.eval`, `ar.claim` (with its judge), `ar.claims` and `ar.reset`. A child run reads its episode
//! once at launch from the file named by `HX_AR_EPISODE` and records it in its frozen policy; everything here reads it
//! from that policy.
use super::*;
use crate::policy::Policy;
use std::sync::Mutex;
use super::files::*;

/// What a child run needs to serve `ar`; written by the orchestrator, named by `HX_AR_EPISODE`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    /// The run directory; sealed from the child's agent.
    pub dir: PathBuf,
    pub label: String,
    /// The spec without what only the orchestrator uses (the `select` command and the `holdout` roots).
    pub spec: Spec,
    /// The evaluators' grader and holdout roots, resolved by the orchestrator, for the child's policy to seal.
    pub sealed: Vec<PathBuf>,
    pub role: Role,
    /// The candidate an exploit worker improves (0 for every other role).
    pub idea: u32,
    /// The snapshot the workspace starts from, and the score an improvement must beat (none: any valid score).
    pub start: String,
    pub best: Option<f64>,
}

/// A worker's progress inside its child run: its claim (and whether one is being judged), its best score so far, and
/// whether an auditor gave its verdict. Held by the run's `Policy`.
#[derive(Default)]
pub struct Progress {
    claim: Option<u32>,
    claiming: bool,
    best: Option<f64>,
    verdict: bool,
}
impl Progress {
    pub fn start(episode: Option<&Episode>) -> Progress {
        Progress { best: episode.and_then(|e| e.best), ..Progress::default() }
    }
}

/// The episode this child `hx run` serves, if the orchestrator started it. Called once at launch; the episode goes into
/// the run's policy, which seals the run directory and installs `ar`.
pub fn load_episode() -> Result<Option<Episode>> {
    let Some(path) = std::env::var_os("HX_AR_EPISODE") else { return Ok(None) };
    let episode: Episode = serde_json::from_slice(&std::fs::read(&path)?).context("HX_AR_EPISODE")?;
    // The orchestrator holds this child's stdin open while it lives: end of input means it is gone, so stop too.
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        crate::prims::stop(1);
    });
    Ok(Some(episode))
}

fn served<'a>(pol: &'a Policy, what: &str) -> Result<(&'a Episode, &'a Mutex<Progress>)> {
    let episode = pol.episode.as_ref().with_context(|| format!("{what} is only available inside an autoresearch episode"))?;
    Ok((episode, &pol.ar_progress))
}

/// `ar.eval(note)` inside an episode: snapshot the editable files, score the snapshot, append to the ledger. Explore and
/// exploit workers score only after their claim is accepted, and their states belong to their candidate.
pub async fn eval(pol: &Policy, note: String) -> Result<Value> {
    let (episode, progress) = served(pol, "ar.eval")?;
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
        // The copy goes inside this episode's scratch copy, beside (not inside) the agent's workspace.
        None => evaluate_in(pol.workspace.parent().filter(|p| valid_scratch(p)), spec, &episode.dir, &files, &spec.eval, false).await?,
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

/// The claim scope of an explore or exploit worker: every explore idea, or the refinements of its candidate.
fn scope(episode: &Episode, what: &str) -> Result<String> {
    Ok(match episode.role {
        Role::Explore => "explore".to_string(),
        Role::Exploit => format!("c{}", episode.idea),
        Role::Research | Role::Audit => bail!("{what} is for explore and exploit workers"),
    })
}

/// The plan an episode label belongs to: a retried attempt `r1-w3.2` is plan `r1-w3`.
fn plan(label: &str) -> &str { label.split('.').next().unwrap_or(label) }

/// Does `claim` still bar a new claim of the worker `label` (starting from snapshot `start`)? A claim made in the same
/// round does: its worker may still be building it. An earlier round's claim does only when its worker scored a valid
/// state; one whose worker stopped before that (its cap, a crash) is free to try again. A refinement (`start` given) is
/// barred only by refinements claimed from the same starting state, so "tune X" can be tried again once the candidate
/// has moved on.
pub(super) fn bars(dir: &Path, rows: &[Row], claim: &Claim, label: &str, start: Option<&str>) -> bool {
    let round = |l: &str| l.split('-').next().unwrap_or("").to_string();
    let same_start = start.is_none_or(|start| valid_label(&claim.worker) && std::fs::read(dir.join("episodes").join(format!("{}.json", claim.worker))).ok()
        .and_then(|b| serde_json::from_slice::<Episode>(&b).ok()).is_some_and(|e| e.start == start));
    same_start && (round(&claim.worker) == round(label) || rows.iter().any(|r| r.valid && plan(&r.episode) == plan(&claim.worker)))
}

/// The claims of `claims` (all in this worker's scope) a new claim must differ from (`bars`).
fn barring(episode: &Episode, claims: &[Claim]) -> Result<Vec<Claim>> {
    let rows = ledger(&episode.dir)?;
    let start = (episode.role == Role::Exploit).then_some(episode.start.as_str());
    Ok(claims.iter().filter(|c| bars(&episode.dir, &rows, c, &episode.label, start)).cloned().collect())
}

/// `ar.claim(idea)`: claim the idea an explore worker will test, or the refinement an exploit worker will try. A claim
/// is rejected only when two independent judge calls both find that it shares the core approach of the same claim that
/// bars it (`bars`), so a borderline idea gets the benefit of the doubt; a judge that cannot answer accepts. The judge
/// runs outside the run's claim lock, against the claims made so far; under the lock the claim is appended unless
/// others arrived meanwhile, which are judged next, again outside the lock. So workers never wait on each other's judge
/// calls (or their rate limits), and a claim is still judged against every claim before it. One accepted claim per
/// worker.
pub async fn claim(host: &crate::host::Host, idea: String) -> Result<Value> {
    let (episode, progress) = served(&host.policy, "ar.claim")?;
    let scope = scope(episode, "ar.claim")?;
    let idea: String = idea.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(1_000).collect();
    // Agents probe a function with a dummy argument ("x"); an accepted claim is final, so a non-description is refused
    // without using up the claim.
    ensure!(idea.split(' ').count() >= 8, "describe the idea in two or three sentences (at least 8 words): an accepted \
claim is final and is how other workers, auditors and the report know what you built");
    {
        let mut p = progress.lock().unwrap();
        if let Some(n) = p.claim { bail!("this worker already claimed #{n}: one idea per worker") }
        ensure!(!p.claiming, "another claim of this worker is being judged; wait for its answer");
        p.claiming = true;
    }
    let result = judge_and_append(host, episode, progress, scope, idea).await;
    progress.lock().unwrap().claiming = false;
    result
}

async fn judge_and_append(host: &crate::host::Host, episode: &Episode, progress: &Mutex<Progress>, scope: String, idea: String) -> Result<Value> {
    let path = episode.dir.join("claims.jsonl");
    let in_scope = |all: Vec<Claim>| -> Vec<Claim> { all.into_iter().filter(|c| c.scope == scope).collect() };
    let mut seen = in_scope(read_lines::<Claim>(&path)?);
    // A retried episode (label `r1-w3.2`) continues the claim an earlier attempt of the same plan made.
    if let Some(mine) = seen.iter().find(|c| plan(&c.worker) == plan(&episode.label)) {
        progress.lock().unwrap().claim = Some(mine.n);
        return Ok(json!({"ok": true, "n": mine.n, "idea": mine.idea, "message": format!("Accepted as #{}: an earlier attempt \
of this worker claimed this idea before it was interrupted. Continue with it: {}", mine.n, mine.idea)}));
    }
    let what = if episode.role == Role::Exploit { "refinement" } else { "core approach" };
    let mut against = barring(episode, &seen)?;
    loop {
        let verdict = judge(host, &against, &idea, episode.role == Role::Exploit).await;
        // Plain text, so a printed result shows every claim (printers abbreviate nested tables).
        let listed = || -> Result<String> { Ok(barring(episode, &seen)?.iter().map(|c| format!("{}. {}\n", c.n, c.idea)).collect()) };
        match verdict {
            Judged::New => {},
            Judged::Same(i) => return Ok(json!({"ok": false, "same_as": against[i].n, "message": format!("Rejected: the same \
{what} as #{} was claimed first. Claimed so far:\n{}Choose a different {what} and claim it.", against[i].n, listed()?)})),
            Judged::Placeholder => return Ok(json!({"ok": false, "message": format!("Rejected: this names no concrete \
{what}. A claim is final and is how everyone knows what you built: claim the {what} you will build. `ar.claims()` lists \
what is taken. Claimed so far:\n{}", listed()?)})),
        }
        let lock_path = episode.dir.join("claims.lock");
        let lock = tokio::task::spawn_blocking(move || lock_file(&lock_path)).await??;
        let now = in_scope(read_lines::<Claim>(&path)?);
        if now.len() == seen.len() {
            let n = now.len() as u32 + 1;
            append_line(&path, &Claim { scope, n, idea, worker: episode.label.clone() })?;
            drop(lock);
            progress.lock().unwrap().claim = Some(n);
            return Ok(json!({"ok": true, "n": n, "message": format!("Accepted as #{n}. Build it now; ar.eval works from here on.")}));
        }
        drop(lock);
        against = barring(episode, &now[seen.len()..])?;
        seen = now;
    }
}

/// What two independent judge calls agree a new claim is.
pub(super) enum Judged {
    New,
    /// The index of the earlier claim it duplicates: the same core approach for explore ideas, the same change for
    /// refinements of one candidate.
    Same(usize),
    /// No concrete approach at all: a placeholder, a probe or a test of the tool.
    Placeholder,
}

/// Judge a new claim against the earlier ones. Only an answer both calls agree on rejects it; anything else accepts.
pub(super) async fn judge(host: &crate::host::Host, earlier: &[Claim], idea: &str, refinement: bool) -> Judged {
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
        // Claims keep their own numbers, which the listing shows: `earlier` need not be every claim of the scope.
        [Some(a), Some(b)] if a == b => earlier.iter().position(|c| c.n as usize == *a).map_or(Judged::New, Judged::Same),
        _ => Judged::New,
    }
}

/// N from the last `Answer: N` in a judge's reply, tolerating markdown such as `**Answer:** #2`; `Answer: none` is
/// `usize::MAX`.
pub(super) fn answer_number(text: &str) -> Option<usize> {
    let at = text.to_ascii_lowercase().rfind("answer")?;
    let rest = text[at + "answer".len()..].trim_start_matches(['*', ':', ' ', '#', '\t']);
    if rest.to_ascii_lowercase().starts_with("none") { return Some(usize::MAX); }
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// `ar.claims()`: the claims a new claim of this worker must differ from (`bars`), as plain text.
pub fn claims(pol: &Policy) -> Result<Value> {
    let (episode, _) = served(pol, "ar.claims")?;
    let scope = scope(episode, "ar.claims")?;
    let all: Vec<Claim> = read_lines::<Claim>(&episode.dir.join("claims.jsonl"))?.into_iter().filter(|c| c.scope == scope).collect();
    let listed: String = barring(episode, &all)?.iter().map(|c| format!("{}. {}\n", c.n, c.idea)).collect();
    Ok(json!({"message": if listed.is_empty() { "No claims yet.".to_string() } else { format!("Claimed so far:\n{listed}") }}))
}

/// `ar.verdict{pass, reason}`: an auditor's verdict on its candidate, final once given, written whole to
/// `episodes/LABEL.verdict.json` (`{"pass": bool, "reason": string}`), where the orchestrator reads it. A fail needs
/// its reason. A typed call rather than a line of prose, so an auditor stopped by its cap after deciding still counts.
pub fn submit_verdict(pol: &Policy, pass: bool, reason: Option<String>) -> Result<Value> {
    let (episode, progress) = served(pol, "ar.verdict")?;
    ensure!(episode.role == Role::Audit, "ar.verdict is for auditors");
    let reason: String = reason.unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(1_000).collect();
    ensure!(pass || !reason.is_empty(), "a failing verdict needs its reason: ar.verdict{{pass = false, reason = \"...\"}}");
    ensure!(valid_label(&episode.label), "invalid episode label {:?}", episode.label);
    let path = episode.dir.join("episodes").join(format!("{}.verdict.json", episode.label));
    let mut p = progress.lock().unwrap();
    ensure!(!p.verdict && !path.exists(), "this auditor already gave its verdict, and a verdict is final");
    crate::atomic_file::replace_trusted(&path, &serde_json::to_vec(&json!({"pass": pass, "reason": reason}))?)?;
    p.verdict = true;
    Ok(json!({"ok": true, "message": "Verdict recorded; it is final. Finish with your final answer."}))
}

/// An exclusive lock on `path`, held until the returned file is dropped: episodes running side by side number ledger
/// rows without gaps or repeats, and claims are appended one at a time, each after every claim it was judged against.
pub(super) fn lock_file(path: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    ensure!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0, "could not lock {}", path.display());
    Ok(file)
}

/// `ar.reset()`: put this worker's best scored state back into the editable paths (the state it started from when it
/// has not improved on it).
pub fn reset(pol: &Policy) -> Result<Value> {
    let (episode, progress) = served(pol, "ar.reset")?;
    let rows = ledger(&episode.dir)?;
    let snap = rows.iter().rev().find(|r| r.improved && r.episode == episode.label)
        .map_or_else(|| episode.start.clone(), |r| r.snap.clone());
    let files = load_snap(&episode.dir, &snap)?;
    // The agent controls this workspace, so every change goes the way its own `write` would: a policy-resolved path,
    // and directories opened without following links.
    place_checked(&std::fs::canonicalize(&pol.workspace)?, &episode.spec, &files, &|rel| pol.check_write(rel))?;
    Ok(json!({"best": progress.lock().unwrap().best}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(n: u32, worker: &str) -> Claim { Claim { scope: "c1".into(), n, idea: format!("idea {n}"), worker: worker.into() } }

    fn row(episode: &str, valid: bool) -> Row {
        Row { n: 1, episode: episode.into(), idea: 1, note: String::new(), snap: "s".into(), score: Some(1.0), valid, se: None,
            improved: false, report: Value::Null }
    }

    #[test]
    fn only_live_claims_bar_a_new_one() {
        let dir = std::env::temp_dir().join(format!("hx-ar-bars-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("episodes")).unwrap();
        let spec: Spec = toml::from_str("editable = [\"x\"]\neval = \"true\"").unwrap();
        for (label, start) in [("r1-w1", "A"), ("r1-w2", "A"), ("r2-w1", "B"), ("r2-w2", "A")] {
            let e = Episode { dir: dir.clone(), label: label.into(), spec: spec.clone(), sealed: vec![], role: Role::Exploit, idea: 1,
                start: start.into(), best: None };
            std::fs::write(dir.join("episodes").join(format!("{label}.json")), serde_json::to_vec(&e).unwrap()).unwrap();
        }
        // r1-w1 scored a valid state (in a retried attempt); r1-w2 never did.
        let rows = vec![row("r1-w1.2", true), row("r1-w2", false)];
        let bars = |c: &Claim, label: &str, start: Option<&str>| bars(&dir, &rows, c, label, start);
        // Explore (no start): an earlier round's claim bars only when its worker scored; this round's always does.
        assert!(bars(&claim(1, "r1-w1"), "r3-w1", None));
        assert!(!bars(&claim(2, "r1-w2"), "r3-w1", None), "a claim whose worker never scored is free again");
        assert!(bars(&claim(2, "r1-w2"), "r1-w5", None), "a claim of this round may still be built");
        // Refinements are barred only by those claimed from the same starting state.
        assert!(bars(&claim(1, "r1-w1"), "r3-w1", Some("A")));
        assert!(!bars(&claim(1, "r1-w1"), "r3-w1", Some("C")), "a refinement can be retried on a newer state");
        assert!(bars(&claim(3, "r2-w2"), "r2-w5", Some("A")) && !bars(&claim(3, "r2-w1"), "r2-w5", Some("A")));
        assert!(!bars(&claim(4, "../x"), "r2-w5", Some("A")), "a malformed worker label names no episode");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn judge_answers() {
        assert_eq!(answer_number("Mechanism: breakout.\nAnswer: 2"), Some(2));
        assert_eq!(answer_number("**Answer:** #3"), Some(3));
        assert_eq!(answer_number("answer: 0"), Some(0));
        assert_eq!(answer_number("It names no approach.\n**Answer:** none"), Some(usize::MAX));
        assert_eq!(answer_number("I think 2 is closest."), None);
    }
}
