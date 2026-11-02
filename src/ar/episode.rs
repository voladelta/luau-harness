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
    pub spec: Spec,
    pub role: Role,
    /// The candidate an exploit worker improves (0 for every other role).
    pub idea: u32,
    /// The snapshot the workspace starts from, and the score an improvement must beat (none: any valid score).
    pub start: String,
    pub best: Option<f64>,
}

/// A worker's progress inside its child run: its claim and its best score so far. Held by the run's `Policy`.
#[derive(Default)]
pub struct Progress {
    claim: Option<u32>,
    best: Option<f64>,
}
impl Progress {
    pub fn start(episode: Option<&Episode>) -> Progress {
        Progress { claim: None, best: episode.and_then(|e| e.best) }
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
    let (episode, progress) = served(&host.policy, "ar.claim")?;
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
        [Some(a), Some(b)] if a == b && (1..=earlier.len()).contains(a) => Judged::Same(a - 1),
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

/// `ar.claims()`: the claims made so far in this worker's scope, as plain text.
pub fn claims(pol: &Policy) -> Result<Value> {
    let (episode, _) = served(pol, "ar.claims")?;
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

#[cfg(test)]
mod tests {
    use super::answer_number;

    #[test]
    fn judge_answers() {
        assert_eq!(answer_number("Mechanism: breakout.\nAnswer: 2"), Some(2));
        assert_eq!(answer_number("**Answer:** #3"), Some(3));
        assert_eq!(answer_number("answer: 0"), Some(0));
        assert_eq!(answer_number("It names no approach.\n**Answer:** none"), Some(usize::MAX));
        assert_eq!(answer_number("I think 2 is closest."), None);
    }
}
