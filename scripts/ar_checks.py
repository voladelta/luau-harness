#!/usr/bin/env python3
"""Offline autoresearch (`hx run --ar`) checks with the real CLI, a scripted provider and a toy evaluator.

The toy task: x.txt holds an integer; the search evaluator scores -(x-7)^2 and the held-out one -(x-5)^2. Candidates
rank by search; the held-out score only vetoes those worse than the seed on it (x of 10 and beyond). Scripted runs exercise the manager loop:
research, ideas planned by the orchestrator (sampled, deduplicated, duplicates sampled again), gates with audits,
exploit rounds and the portfolio. The isolation checks replay attacks an independent review found against the first
design: each must leave the recorded scores honest.
"""
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
import redesign_checks as harness
from pathlib import Path

BASE = harness.BASE / "ar"
BASE.mkdir(exist_ok=True)
SCORE = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": -(x - %d) ** 2, "report": {"x": x}}))'
passed = 0
lock = threading.Lock()


def check(name):
    global passed
    with lock:
        passed += 1
        print(f"ok {passed} {name}", flush=True)


def workspace(name, files=None, spec=None):
    path = BASE / name
    path.mkdir(parents=True)
    git = lambda *a: subprocess.run(["git", *a], cwd=path, check=True, capture_output=True)
    git("init", "-q")
    git("config", "user.email", "test@test.local")
    git("config", "user.name", "test")
    (path / ".harness").mkdir()
    (path / ".gitignore").write_text(".harness/\n")
    files = files or {"x.txt": "0\n", "search.py": SCORE % 7, "select.py": SCORE % 5}
    for rel, text in files.items():
        (path / rel).parent.mkdir(parents=True, exist_ok=True)
        (path / rel).write_text(text)
    (path / ".harness/ar.toml").write_text(spec or 'editable = ["x.txt"]\neval = "python3 search.py"\nselect = "python3 select.py"\ntimeout = 30\n')
    (path / "program.md").write_text("Make the score as high as possible.")
    git("add", "-A")
    git("commit", "-qm", "init")
    return path


def command(path, url, key="HX_TEST_KEY"):
    return [str(harness.HX), "run", "--ar", "--workspace", str(path), "--prompt-file", str(path / "program.md"),
            "--model", "mock", "--base-url", url, "--key-env", key]


def ar(path, url, ok=True, env=None, key="HX_TEST_KEY", extra=()):
    result = subprocess.run(command(path, url, key) + list(extra), env={**harness.ENV, **(env or {})},
                            capture_output=True, text=True, timeout=600)
    (BASE / f"{path.name}.stderr").write_text(result.stderr)
    if ok:
        assert result.returncode == 0, (result.returncode, result.stdout[-2000:], result.stderr[-4000:])
    return result


def ar_runs(path):
    """A workspace's autoresearch runs: in hx's state directory, outside the workspace."""
    return harness.state_dir(path) / "ar"


def confined_off(path):
    """Did this workspace's runs record that Landlock is unavailable (shell commands run unconfined)?"""
    return any('"confinement":"off' in p.read_text() for p in ar_runs(path).rglob("*.jsonl"))


def report_of(result):
    return json.loads(result.stdout.strip().splitlines()[-1])


def records_of(run):
    """The run's journal: one hash-chained record per line (a torn final line is skipped)."""
    return [json.loads(l) for l in (run / "journal.jsonl").read_text().splitlines() if l.endswith("}")]


def blob(run, sha):
    return (run / "blobs" / sha).read_bytes()


def snapshot(run, snap):
    """A stored snapshot as {path: bytes}: its manifest (a blob) names each file's mode and content-addressed blob."""
    return {rel: blob(run, entry["blob"]) for rel, entry in json.loads(blob(run, snap)).items()}


def label(r):
    """The label of an episode record's attempt."""
    return r["plan"] if r["attempt"] == 1 else f"{r['plan']}.{r['attempt']}"


def started(records):
    return [label(r) for r in records if r["kind"] == "episode" and r["outcome"] == "started"]


def ended(records):
    return [(label(r), r["outcome"]) for r in records if r["kind"] == "episode" and r["outcome"] != "started"]


def evals(records):
    """The evaluations with their search scores, as rows: {n, episode, candidate, snap, note, score, valid, se}."""
    scores = {}
    for r in records:
        if r["kind"] == "score" and r["tier"] == "search": scores.setdefault(r["snap"], r)
    return [{**{k: scores[r["snap"]][k] for k in ("score", "valid", "se")}, **{k: r[k] for k in ("n", "episode", "candidate", "snap", "note")}}
            for r in records if r["kind"] == "eval"]


def selects(records):
    return {r["snap"]: r["score"] if r["valid"] else None for r in records if r["kind"] == "score" and r["tier"] == "select"}


def plans(records, batch=None):
    return [p for r in records if r["kind"] == "planned" and batch in (None, r["batch"]) for p in r["episodes"]]


def ledger(path, report):
    return evals(records_of(ar_runs(path) / report["run"]))


def answered(body):
    return "function_call_output" in json.dumps(body) or '"role": "tool"' in json.dumps(body)


def text_of(body):
    """Every string in a request, newlines intact: the system prompt, the task and any tool outputs."""
    out = []
    def walk(v):
        if isinstance(v, str): out.append(v)
        elif isinstance(v, dict): [walk(x) for x in v.values()]
        elif isinstance(v, list): [walk(x) for x in v]
    walk(body)
    return "\n".join(out)


def role_of(body):
    t = text_of(body)
    if "You compare research ideas" in t: return "dedupe", None
    for role, marker in (("idea", "## Autoresearch idea for "), ("research", "## Autoresearch research lead ("),
                         ("explore", "## Autoresearch explore worker "), ("exploit", "## Autoresearch exploit worker "),
                         ("audit", "## Autoresearch auditor ")):
        if marker in t:
            return role, t.split(marker, 1)[1].split(None, 1)[0].rstrip(":)")
    return "other", None


def taken_in(body):
    """The ideas an idea call lists as taken."""
    t = text_of(body)
    return t.split("These are taken", 1)[1].split("End with one line", 1)[0] if "These are taken" in t else ""


def idea_in(body):
    """The idea or refinement planned for a worker, from its task."""
    t = text_of(body)
    m = re.search(r"<(?:idea|refinement)>(.*?)</(?:idea|refinement)>", t, re.S)
    return m.group(1).strip() if m else ""


def dedupe(body):
    """A dedupe call that finds two ideas the same when their text is identical."""
    t = text_of(body)
    taken = re.findall(r"^T\d+\. (.*)$", t.split("New, in order:")[0], re.M)
    new = re.findall(r"^(\d+)\. (.*)$", t.split("New, in order:")[1].split("Which new ones")[0], re.M)
    seen, dupes = set(taken), []
    for n, idea in new:
        if idea.strip() in seen: dupes.append(n)
        seen.add(idea.strip())
    return f"Each names its mechanism.\nDuplicates: {', '.join(dupes) or 'none'}"


def provider(reply, ideas=None):
    """A scripted endpoint whose idea calls answer with `ideas(body, label)` (default: an idea of its own per worker) and
    whose dedupe calls find identical ideas the same; every other request goes to `reply`."""
    def wrapped(body, index):
        role, label = role_of(body)
        if role == "idea": return (ideas or (lambda b, l: f"Dimensions: none.\nIDEA: the idea planned for worker {l}"))(body, label)
        if role == "dedupe": return dedupe(body)
        return reply(body, index)
    return harness.provider(wrapped)


def once(code, where=("research",)):
    """A provider reply whose agent runs `code` once in the named roles or episode labels and finishes; every other
    episode finishes at once."""
    def reply(body, index):
        role, label = role_of(body)
        if answered(body) or (role not in where and label not in where): return "done"
        return {"code": code}
    return reply


def outputs(requests):
    return [item.get("output", "") for body in requests for item in body.get("input", []) if item.get("type") == "function_call_output"]


def verdict_code(passed, reason=""):
    return {"code": f'ar.verdict{{pass = {str(passed).lower()}, reason = {json.dumps(reason)}}}\nreturn "RECORDED"'}


def audit(body, passed, reason=""):
    """An auditor's reply: its verdict through `ar.verdict{pass, reason}` (passed=None: it gives none)."""
    if passed is None: return "Checked the state; ran out of steps before deciding."
    if not answered(body): return verdict_code(passed, reason)
    return "Checked the state."


def journal_of(path, report=None):
    run = ar_runs(path) / report["run"] if report else sorted(ar_runs(path).glob("*/journal.jsonl"))[-1].parent
    return records_of(run)


def snap_x(path, report, snap):
    """x of a snapshot of the toy task."""
    return int(snapshot(ar_runs(path) / report["run"], snap)["x.txt"])


def workers(n, rounds=None, extra=""):
    """A toy spec with `n` workers per round (and at most `rounds` rounds), for scenarios that need few episodes."""
    return (f'editable = ["x.txt"]\neval = "python3 search.py"\nselect = "python3 select.py"\ntimeout = 30\nworkers = {n}\n'
            + (f"max_rounds = {rounds}\n" if rounds else "") + extra)


AUDIT_GATE = BASE / "resume-audit-gate"


def held_out_roots():
    """A grader root whose score script reports whether it can read a held-out root, made once for the scenarios that
    share it."""
    grader, holdout = BASE / "g-grader", BASE / "g-holdout"
    with lock:
        if not grader.exists():
            grader.mkdir()
            holdout.mkdir()
            (grader / "score.sh").write_text('if cat "$1/secret.txt" >/dev/null 2>&1; then echo \'{"score": 1}\'; else echo \'{"score": 0}\'; fi\n')
            (holdout / "secret.txt").write_text("held-out")
    return grader, holdout

# 1. The manager loop: research, explore workers each with a planned idea (a duplicate is sampled again), a gate with an
# audit, exploit rounds with distinct refinements, and a portfolio of distinct audited candidates ranked by held-out score.
TARGETS = {1: 3, 2: 6, 3: 20, 4: 6, 5: 9, 6: 1, 7: 12, 8: 13}  # workers 2 and 4 get the same idea: one of them must be sampled again
UP, DOWN, HOLD = "step up by adding one to the current value of x", "step down by subtracting one from the current value of x", "hold x where it is"
def go(x):
    return f"go to {x} by writing that one value into x.txt"


def scripted_idea(body, label):
    """Each worker's idea is its lane's target unless the call lists it as taken; then the next free one. A refinement
    is a step up, else a step down, else holding."""
    rnd, w = int(label[1:].split("-")[0]), int(label.split("-w")[1].split(".")[0])
    taken = taken_in(body)
    if "## Autoresearch idea" in text_of(body) and "Candidate #" in text_of(body).split("## Autoresearch idea", 1)[1]:
        return "IDEA: " + next(r for r in (UP, DOWN, HOLD) if r == HOLD or r not in taken)
    return "IDEA: " + next(go(x) for x in (TARGETS[w], 40 + w, 50 + 10 * rnd + w) if go(x) not in taken)


def scripted(body, index):
    role, label = role_of(body)
    if role == "audit":
        return audit(body, False, "probe trick") if f"({go(3)})" in text_of(body) else audit(body, True)
    if answered(body):
        return f"FINDING from {label}: done"
    if role == "research":
        return "BRIEF: the score rewards x close to the hidden target; avoid huge x."
    if role == "explore":
        x = int(re.search(r"go to (\d+) by", idea_in(body)).group(1))
        return {"code": f'write("x.txt", "{x}\\n")\nprint(ar.eval("go " .. {x}))\n'
                        f'write("x.txt", "{100 + index}\\n")\nprint(ar.eval("probe"))\nar.reset()\nreturn "RESET " .. {x} .. " " .. read("x.txt")'}
    # Only round 2 refines: a moved candidate could take the same step again, and round 3 finds nothing to do.
    if role == "exploit" and label.startswith("r2-"):
        d = {UP: 1, DOWN: -1}.get(idea_in(body), 0)
        if not d: return "nothing left to refine"
        return {"code": f'local x = tonumber(read("x.txt"))\nwrite("x.txt", tostring(x + {d}) .. "\\n")\nprint(ar.eval("step {d}"))\nreturn "STEP {d}"'}
    return "done"


# 1b. While refining keeps making progress, rounds go on; the score tops out at 12, and then the run stops by itself.
CLIMB = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": min(x, 12)}))'
def climbing(body, index):
    role, label = role_of(body)
    if role == "audit": return audit(body, True)
    if answered(body): return f"FINDING from {label}: done"
    if role == "explore":
        # Three ideas work (more would keep the run climbing until its round cap); the others build nothing valid.
        w = int(label.split("-w")[1].split(".")[0])
        x = w + 6 if w <= 3 else "nope"
        return {"code": f'write("x.txt", "{x}\\n")\nreturn ar.eval("go {w}")'}
    if role == "exploit":
        return {"code": 'write("x.txt", tostring(tonumber(read("x.txt")) + 1) .. "\\n")\nreturn ar.eval("up")'}
    return "done"


# 2. Ideas below the seed still go on to refinement, but never become results: the workspace keeps the seed. With no
# valid idea at all the run stops after round 1.
def hopeless(body, index, x=None):
    role, label = role_of(body)
    if answered(body) or role != "explore": return "done"
    w = int(label.split("-w")[1].split(".")[0])
    value = x or ("00" if w == 1 else 30 + w)  # worker 1 rebuilds the seed in other bytes
    return {"code": f'write("x.txt", "{value}\\n")\nreturn ar.eval("far")'}


# 7. A killed run resumes from its journal: the interrupted worker is retried with its planned idea, the rest of the plan
# continues.
def slow(body, index):
    role, label = role_of(body)
    if role == "explore" and label == "r1-w6" and not answered(body):
        return {"code": 'sh("sleep 20")\nreturn "late"'}
    return scripted(body, index)


# 7b. A run killed during an audit re-runs that audit on resume; no candidate passes unaudited.
def slow_audit(body, index):
    role, label = role_of(body)
    if role == "audit" and label.startswith("a1-") and not answered(body) and AUDIT_GATE.exists() is False:
        AUDIT_GATE.write_text("")
        return {"code": 'sh("sleep 20")\nreturn "late"'}
    return scripted(body, index)


# 9. Provider crashes are charged from the child's journal and retried; misconfiguration fails fast and nonzero.
agent_calls = [0]
def limited(body, index):
    # Unavailable twice in a row exhausts the one retry, so the research child crashes (rate limits are waited out instead).
    if role_of(body)[0] == "research" and not answered(body):
        agent_calls[0] += 1
        if agent_calls[0] < 3: return {"status": 503, "error": "unavailable"}
    return once('return ar.eval("b")')(body, index)


def scenario_00():
    # 1. The manager loop: research, explore workers each with a planned idea (a duplicate is sampled again), a gate with
    # an audit, exploit rounds with distinct refinements, and a portfolio of distinct audited candidates ranked by
    # held-out score.
    path = workspace("rounds")
    with provider(scripted, scripted_idea) as (url, requests):
        result = ar(path, url)
    report = report_of(result)
    run = ar_runs(path) / report["run"]
    records = records_of(run)
    rows = evals(records)
    explore = plans(records, "round1")
    assert [p["candidate"] for p in explore] == list(range(1, 9)) and all(p["role"] == "explore" for p in explore), explore
    ideas = [p["idea"] for p in explore]
    assert ideas.count(go(6)) == 1 and len(set(ideas)) == 8, ideas
    loser = 2 if next(p for p in explore if p["idea"] == go(6))["label"] == "r1-w4" else 4
    assert go(40 + loser) in ideas, (loser, ideas)
    asked = [(role_of(b)[1], taken_in(b)) for b in requests if role_of(b)[0] == "idea" and role_of(b)[1].startswith("r1-")]
    assert sorted(l for l, t in asked if not t) == [f"r1-w{w}" for w in range(1, 9)], asked  # one call per worker, side by side
    assert [l for l, t in asked if t] == [f"r1-w{loser}"] and go(6) in asked[-1][1], asked  # the duplicate, sampled again
    assert sum(role_of(b)[0] == "dedupe" and "Same core" in text_of(b) for b in requests) >= 1
    check("ideas are planned per worker and deduplicated: a duplicate is sampled again with the taken ideas listed")

    assert any(b for b in requests if role_of(b) == ("research", "r0-research")) and "BRIEF:" in (run / "episodes/r1-w1.md").read_text()
    gates = [r for r in records if r["kind"] == "gate" and not r["last"]]
    assert [g["round"] for g in gates] == [1, 2, 3, 4], gates
    three = next(p["candidate"] for p in explore if p["idea"] == go(3))
    six = next(p["candidate"] for p in explore if p["idea"] == go(6))
    assert gates[0]["chosen"][0] == six and three not in gates[0]["chosen"] and len(gates[0]["chosen"]) == 3, gates[0]
    of = {r["snap"]: r["candidate"] for r in rows}
    assert any(r["kind"] == "verdict" and of.get(r["snap"]) == three and not r["pass"] for r in records)
    assert not any(three in g["chosen"] for g in gates)
    bad = {p["candidate"] for p in explore if p["idea"] in (go(20), go(40 + loser), go(12), go(13))}
    vetoed = [r for r in rows if r["candidate"] in bad and r["valid"] and r["score"] > -49]
    assert vetoed and all(selects(records)[r["snap"]] < -25 for r in vetoed), vetoed  # beat the seed on search, lose on held-out
    assert all(not c["good"] and c["frontier"]["snap"] not in {r["snap"] for r in vetoed}
               for c in report["candidates"] if c["candidate"] in bad), report["candidates"]
    check("research briefs every worker; the gate keeps good candidates, audits them and drops the one that fails")

    for g in gates[:1]:
        for c in g["chosen"]:
            refinements = [p["idea"] for p in plans(records, "round2") if p["candidate"] == c and p["idea"] != HOLD]
            assert refinements and len(refinements) == len(set(refinements)) and set(refinements) <= {UP, DOWN}, (c, refinements)
    assert {p["role"] for p in plans(records, "round2")} == {"exploit"}
    assert any(role_of(b)[0] == "dedupe" and "Same means the same change" in text_of(b) for b in requests)
    check("exploit rounds split workers across the chosen candidates, each with a distinct planned refinement")

    # Round 3 finds no refinement left, so round 4 explores again; its idea calls list every earlier idea as taken and get
    # new ones. Round 4's new ideas don't move the chosen candidates either, so the run stops.
    roles = {r["batch"]: r["episodes"][0]["role"] for r in records if r["kind"] == "planned" and r["batch"].startswith("round")}
    assert roles == {"round1": "explore", "round2": "exploit", "round3": "exploit", "round4": "explore"}, roles
    everything = [p["idea"] for p in plans(records) if p["role"] == "explore"]
    assert len(everything) > 8 and len(set(everything)) == len(everything), everything
    r4 = next(taken_in(b) for b in requests if role_of(b) == ("idea", "r4-w1"))
    assert all(p["idea"] in r4 for p in explore), r4
    assert "These are taken" not in text_of(next(b for b in requests if role_of(b) == ("idea", "r1-w1")))
    assert "two rounds in a row made no progress" in result.stderr, result.stderr[-1500:]
    check("a refining round that moves no chosen candidate is followed by an explore round; two rounds without progress stop the run")

    # Replaying the finished run from its journal reaches the same stop without starting anything.
    (run / "report.json").unlink()
    before = (run / "journal.jsonl").read_text()
    with provider(scripted, scripted_idea) as (url, replayed):
        replay = subprocess.run([str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)],
                                env=harness.ENV, capture_output=True, text=True, timeout=600)
    assert replay.returncode == 0 and not replayed, replay.stderr[-2000:]
    assert (run / "journal.jsonl").read_text() == before, "the replay journaled something"
    again = report_of(replay)
    assert (again["rounds"], again["snap"], again["portfolio"]) == (report["rounds"], report["snap"], report["portfolio"]), again
    check("a resumed run replays its rounds from the journal: same decisions, nothing re-planned or re-run")

    portfolio = report["portfolio"]
    assert 1 <= len(portfolio) <= 3 and len({p["snap"] for p in portfolio}) == len(portfolio), portfolio
    passed = {r["snap"] for r in records if r["kind"] == "verdict" and r["pass"]}
    assert all(p["snap"] in passed and p["candidate"] != three for p in portfolio), portfolio
    assert [p["search"] for p in portfolio] == sorted((p["search"] for p in portfolio), reverse=True), portfolio
    assert all(p["search"] > -49 and p["select"] >= -25 for p in portfolio), portfolio  # nothing the held-out veto rejects
    x = int((path / "x.txt").read_text())
    assert report["select"] == portfolio[0]["select"] == -(x - 5) ** 2, (x, report["select"], portfolio)
    assert all((run / "portfolio" / f"c{p['candidate']}" / "x.txt").exists() for p in portfolio)
    check("the result is a portfolio of distinct audited candidates the held-out score did not veto, best search first")

    assert [r["n"] for r in rows] == list(range(1, len(rows) + 1)), [r["n"] for r in rows]
    for r in rows:
        snap = snapshot(run, r["snap"])
        assert r["score"] == -(int(snap["x.txt"]) - 7) ** 2, r
    resets = [o for o in outputs(requests) if o.startswith("RESET ") or "RESET " in o]
    assert len(resets) >= 8 and all(o.split("RESET ")[1].split()[0] == o.split("RESET ")[1].split()[1] for o in resets), resets
    check("evaluations stay numbered under concurrency, every score matches its snapshot, and ar.reset is per worker")


def scenario_01():
    # 1b. While refining keeps making progress, rounds go on; the score tops out at 12, and then the run stops by itself.
    # Each refining round adds 1, below min_delta = 1.5: progress only counts once two steps add up, so a run that judged
    # each round alone would stop after round 3.
    path = workspace("climb", {"x.txt": "0\n", "search.py": CLIMB, "select.py": CLIMB}, workers(5, extra="min_delta = 1.5\n"))
    with provider(climbing) as (url, _):
        result = ar(path, url)
    report = report_of(result)
    assert report["rounds"] > 4 and "two rounds in a row made no progress" in result.stderr, (report["rounds"], result.stderr[-1500:])
    assert report["search"] == 12, report["search"]
    check("small refinement steps add up to progress; a run that keeps progressing goes on until progress stops")

    records = journal_of(path, report)
    widths = [len(r["episodes"]) for r in records if r["kind"] == "planned" and r["batch"].startswith("round")]
    assert set(widths) == {5}, widths
    # The candidate that passed its audit early climbed past it by more than min_delta: the last gate audited the lift.
    assert any(r["kind"] == "planned" and r["batch"] == "audit-last" for r in records), [r.get("batch") for r in records if r["kind"] == "planned"]
    check("the spec sets the workers per round; the last gate audits a result's lift beyond the margin")


def scenario_02():
    # 2. Ideas below the seed still go on to refinement, but never become results: the workspace keeps the seed. With no
    # valid idea at all the run stops after round 1.
    path = workspace("hopeless")
    before = (path / "x.txt").stat().st_mtime_ns
    with provider(hopeless) as (url, _):
        result = ar(path, url)
    report = report_of(result)
    run = ar_runs(path) / report["run"]
    records = records_of(run)
    gates = [r for r in records if r["kind"] == "gate"]
    # Later rounds build nothing, so the last gate's decisions are the first gate's.
    candidates = report["candidates"]
    assert len(gates[0]["chosen"]) == 3 and not any(c["good"] for c in candidates), (gates[0], candidates)
    copy = next(c for c in candidates if c["frontier"]["search"] == -49)
    first = [c["candidate"] for c in sorted(candidates, key=lambda c: (-c["frontier"]["search"], c["candidate"]))][:3]
    assert copy["candidate"] == gates[0]["chosen"][0] and not copy["good"], gates[0]  # a tie with the seed: not good, ranked by search
    assert gates[0]["chosen"] == first, (gates[0]["chosen"], first)
    round2 = plans(records, "round2")
    assert {p["role"] for p in round2} == {"exploit"} and {p["candidate"] for p in round2} == set(first), round2
    assert report["portfolio"] == [] and (path / "x.txt").read_text() == "0\n" and "no candidate beat the seed" in result.stderr, report
    assert (path / "x.txt").stat().st_mtime_ns == before, "the workspace was rewritten although the result is the seed"
    assert not any(p.name.startswith(("a", "f")) for p in (run / "episodes").iterdir()), "a candidate below the seed was audited"
    check("ideas below the seed (and a tie with it) go on to refinement unaudited; a seed result leaves the workspace untouched")

    path = workspace("invalid")
    with provider(lambda body, index: hopeless(body, index, "nope")) as (url, _):
        result = ar(path, url)
    report = report_of(result)
    assert report["rounds"] == 1 and report["portfolio"] == [] and (path / "x.txt").read_text() == "0\n", report
    check("with no valid idea after round 1 the run stops and the workspace keeps the seed")


def scenario_03():
    # 2c. An auditor that gives no verdict leaves its candidate unaudited: nothing becomes a result without a passing audit.
    path = workspace("noverdict")
    with provider(lambda body, index: audit(body, None) if role_of(body)[0] == "audit" else scripted(body, index), scripted_idea) as (url, _):
        report = report_of(ar(path, url))
    records = journal_of(path, report)
    assert any(r["kind"] == "planned" and r["batch"].startswith("audit") for r in records), "no audit ran"
    assert not any(r["kind"] == "verdict" for r in records) and report["portfolio"] == [], report["portfolio"]
    assert (path / "x.txt").read_text() == "0\n" and "no verdict; it stays unaudited" in (BASE / "noverdict.stderr").read_text()
    check("an auditor that gives no verdict leaves its candidate unaudited, never a result")


def scenario_04():
    # 3. A select evaluator that cannot score the seed stops the run before any episode.
    path = workspace("badselect", spec='editable = ["x.txt"]\neval = "python3 search.py"\nselect = "exit 3"\ntimeout = 30\n')
    with provider(scripted, scripted_idea) as (url, requests):
        result = ar(path, url, ok=False)
    assert result.returncode != 0 and "select evaluator gives no valid score" in result.stderr and not requests, result.stderr[-800:]
    check("a broken select evaluator fails fast, before spending")


def scenario_05():
    # 2. Tampering outside the editable paths never reaches the evaluator: no Git hooks, no ignore rules, no evaluator edits.
    tamper = (f'sh("git init -q; git config core.fsmonitor $PWD/hook.sh; printf \'#!/bin/sh\\\\ntouch {BASE}/pwned\\\\n\' > hook.sh; chmod +x hook.sh; echo json.py >> .git/info/exclude")\n'
              'write("search.py", "print(\'{\\"score\\": 1000}\')")\n'
              'write("json.py", "def dumps(o): return \'{\\"score\\": 999}\'\\n")\n'
              'return ar.eval("tamper")')
    for mode in ("default", "autopilot"):
        path = workspace(f"tamper-{mode}")
        with provider(once(tamper)) as (url, _):
            report = report_of(ar(path, url, extra=["--yes"] if mode == "autopilot" else []))
        rows = ledger(path, report)
        assert rows and all(r["score"] == -49 for r in rows), rows
        assert (path / "search.py").read_text() == SCORE % 7
        log = subprocess.run(["git", "log", "--oneline"], cwd=path, capture_output=True, text=True).stdout
        assert len(log.splitlines()) == 1, log
    assert not (BASE / "pwned").exists(), "a hook ran"
    check("tampering outside the editable paths cannot change scores or the owner's repository (default and autopilot)")


def scenario_06():
    # Editable code in evaluators gets no inherited credentials or episode descriptor.
    files = {"cand.py": "value = 0\n",
             "search.py": 'import json, cand; print(json.dumps({"score": 0, "report": {"leak": repr(cand.value)}}))'}
    path = workspace("envleak", files, 'editable = ["cand.py"]\neval = "python3 search.py"\ntimeout = 30\n')
    snoop = ('write("cand.py", "import os\\nvalue = [os.environ.get(\'HX_TEST_KEY\'), os.environ.get(\'HX_AR_EPISODE\')]\\n")\n'
             'return ar.eval("snoop")')
    with provider(once(snoop)) as (url, requests):
        ar(path, url)
    leak = [o for o in outputs(requests) if "leak" in o]
    assert leak and "[None, None]" in leak[0], leak
    check("evaluators run with a scrubbed environment")


def scenario_07():
    # File tools cannot read owner state in either policy mode, and shell commands cannot see it (nothing of the run
    # store is in the episode's copy; the run directory is hidden by Landlock), nor the episode's channel. Each episode's
    # journal is kept with the run.
    for mode in ("default", "autopilot"):
        path = workspace(f"sealed-{mode}")
        look = (f'local r = sh("ls -d .harness/log 2>/dev/null | wc -l; cat {ar_runs(path)}/*/journal.jsonl 2>/dev/null | wc -c; ls -l /proc/self/fd | grep -c socket")\n'
                f'local ok, e = pcall(read, "{ar_runs(path)}")\n'
                'print(ar.eval("baseline"))\n'
                'return "OUT " .. r.stdout .. " READ " .. tostring(e)')
        with provider(once(look, ("research", "r1-w1"))) as (url, requests):
            report = report_of(ar(path, url, extra=["--yes"] if mode == "autopilot" else []))
        outs = [o for o in outputs(requests) if "OUT " in o]
        assert len(outs) >= 2, outs
        assert all("sealed owner state" in o for o in outs), outs
        assert all(o.split("OUT ")[1].split("READ")[0].split()[:2] == ["0", "0"] for o in outs) or confined_off(path), outs
        # No shell command holds a socket: the episode's channel never reaches one.
        assert all(o.split("OUT ")[1].split("READ")[0].split()[2] == "0" for o in outs), outs
        logs = sorted((ar_runs(path) / report["run"] / "episodes").glob("*.log"))
        assert {p.name for p in logs} >= {"r0-research.log", "r1-w1.log"} and all(len(list(p.glob("*.jsonl"))) == 1 for p in logs), logs
    check("file tools and shell commands cannot reach autoresearch state or the episode's channel; episode journals are kept")


def scenario_08():
    # 5. A late writer cannot change what is scored: the evaluator reads a private copy of the snapshot.
    race = ('write("d.py", "import os, time\\nif os.fork() == 0:\\n    os.setsid()\\n    time.sleep(0.3)\\n    open(\'x.txt\',\'w\').write(\'7\\\\n\')\\n    os._exit(0)\\n")\n'
            'sh("python3 d.py >/dev/null 2>&1 </dev/null")\nreturn ar.eval("honest x=0")')
    path = workspace("race", spec='editable = ["x.txt"]\neval = "sleep 1; python3 search.py"\ntimeout = 30\n')
    with provider(once(race)) as (url, _):
        report = report_of(ar(path, url))
    first = ledger(path, report)[0]
    snap = snapshot(ar_runs(path) / report["run"], first["snap"])
    assert snap["x.txt"] == b"0\n" and first["score"] == -49, (first, snap)
    check("snapshot and score always agree")


def scenario_09():
    # 6. Binary files and symlinks in an editable directory are carried or skipped, never fatal.
    files = {"pkg/a.txt": "0\n", "search.py": 'import json, os; print(json.dumps({"score": int(os.path.exists("pkg/blob.bin"))}))'}
    path = workspace("binary", files, 'editable = ["pkg"]\neval = "python3 search.py"\ntimeout = 30\n')
    junk = 'sh("printf \'\\\\377\\\\376\' > pkg/blob.bin; ln -s /etc/hosts pkg/link")\nreturn ar.eval("junk")'
    with provider(once(junk)) as (url, _):
        report = report_of(ar(path, url))
    snap = snapshot(ar_runs(path) / report["run"], ledger(path, report)[0]["snap"])
    assert snap["pkg/blob.bin"] == b"\xff\xfe" and "pkg/link" not in snap, list(snap)
    check("binary editable files survive and symlinks are skipped")


def copies(run, label):
    """An episode attempt's copies in the temp directory, as the orchestrator names them."""
    stem = run.name.removesuffix(".noindex")
    return [p for p in Path(tempfile.gettempdir()).glob(f"hx-ar-ws-*-{stem}-{label}.noindex")]


def scenario_10():
    # 7. A killed run resumes from its journal: the interrupted worker is retried with its planned idea, the rest of the
    # plan continues.
    path = workspace("resume")
    with provider(slow, scripted_idea) as (url, _):
        process = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        deadline, journal = time.time() + 180, None
        while time.time() < deadline:
            journals = list(ar_runs(path).glob("*/journal.jsonl"))
            if journals:
                lines = records_of(journals[0].parent)
                done = {l for l, _ in ended(lines)}
                if sum(l.startswith("r1-") for l in done) >= 7 and "r1-w6" not in done and "r1-w6" in started(lines):
                    journal = journals[0]
                    break
            time.sleep(0.05)
        assert journal, "never caught round 1 with a worker in flight"
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        assert copies(journal.parent, "r1-w6"), "the killed worker's copy is not where a resume looks"
        with open(journal, "a") as f:
            f.write('{"kind": "fini')
        (path / ".harness/ar.toml").write_text((path / ".harness/ar.toml").read_text() + "# edited after launch\n")
        # The edited ar.toml is not trusted; resume runs on the launch record, frozen in the run's journal out of every agent's reach.
        untrusted = {k: v for k, v in harness.ENV.items() if k != "HX_TRUST"}
        result = subprocess.run([str(harness.HX), "run", "--ar", "--resume", str(journal.parent), "--workspace", str(path)],
                                env=untrusted, capture_output=True, text=True, timeout=600)
        assert result.returncode == 0, result.stderr[-3000:]
    report = report_of(result)
    records = records_of(journal.parent)
    assert [l for l, o in ended(records) if o == "interrupted"] == ["r1-w6"], ended(records)
    assert not copies(journal.parent, "r1-w6"), "killed episode's scratch copy left behind"
    assert (journal.parent / "episodes/r1-w6.log").is_dir(), "the killed episode's journal was not kept"
    assert "r1-w6.2" in started(records), "the interrupted worker was not retried"
    mine = next(p for p in plans(records, "round1") if p["label"] == "r1-w6")
    assert any(r["episode"] == "r1-w6.2" and r["candidate"] == mine["candidate"] for r in evals(records)), "the retry did not keep its idea"
    assert sum(r["kind"] == "planned" and r["batch"] == "round1" for r in records) == 1 and report["rounds"] >= 3
    assert "resumed" in result.stderr
    check("a killed run resumes past a torn line with its frozen spec, closing and retrying the interrupted worker")


def scenario_11():
    # 7b. A run killed during an audit re-runs that audit on resume; no candidate passes unaudited.
    path = workspace("resume-audit")
    with provider(slow_audit, scripted_idea) as (url, _):
        process = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        deadline, journal = time.time() + 180, None
        while time.time() < deadline:
            journals = list(ar_runs(path).glob("*/journal.jsonl"))
            if journals and AUDIT_GATE.exists():
                if any(l.startswith("a1-") for l in started(records_of(journals[0].parent))):
                    journal = journals[0]
                    break
            time.sleep(0.05)
        assert journal, "never caught an audit in flight"
        time.sleep(1)
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        result = subprocess.run([str(harness.HX), "run", "--ar", "--resume", str(journal.parent), "--workspace", str(path)],
                                env=harness.ENV, capture_output=True, text=True, timeout=600)
        assert result.returncode == 0, result.stderr[-3000:]
    records = records_of(journal.parent)
    assert any(r["kind"] == "verdict" for r in records)
    assert sum(r["kind"] == "planned" and r["batch"] == "audit1" for r in records) == 1
    rerun = [l for l in started(records) if l.startswith("a1-") and "." in l]
    assert rerun, started(records)
    check("a run killed during an audit re-runs it on resume; no candidate passes unaudited")


def scenario_12():
    # 7c. Stopping the orchestrator stops everything it started: episode children and the commands they run.
    path = workspace("stop")
    marker = f"sleep 47{os.getpid() % 100}"
    with provider(once(f'sh("{marker}")\nreturn "late"')) as (url, _):
        process = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.time() + 60
        while time.time() < deadline and marker not in subprocess.run(["ps", "-Ao", "command"], capture_output=True, text=True).stdout:
            time.sleep(0.1)
        assert time.time() < deadline, "the research episode never started its command"
        process.terminate()  # SIGTERM to the orchestrator alone, as a stop button sends
        process.wait(timeout=30)
        time.sleep(2)
        left = [l for l in subprocess.run(["ps", "-Ao", "command"], capture_output=True, text=True).stdout.splitlines()
                if marker in l or (str(path) in l and "hx" in l)]
    assert not left, left
    check("stopping the orchestrator stops its episode children and the commands they run")


def scenario_13():
    # 8. Agents that never score anything end the run after round 1, and a run with no evals fails.
    path = workspace("idle")
    with provider(lambda body, index: "nothing to do") as (url, _):
        result = ar(path, url, ok=False)
    episodes = list(ar_runs(path).glob("*/episodes/*.md"))
    assert result.returncode != 0 and "made no evaluations" in result.stderr and len(episodes) == 9, (len(episodes), result.stderr[-800:])
    check("idle agents end the run after round 1; a run without evals fails")


def scenario_14():
    # 9. Provider crashes are charged from the child's journal and retried; misconfiguration fails fast and nonzero.
    path = workspace("crash")
    with provider(limited) as (url, _):
        result = ar(path, url, env={"HX_AR_RETRY_SECS": "0"})
    report = report_of(result)
    assert "retrying" in result.stderr and report["evals"] > 0, result.stderr[-1500:]
    records = journal_of(path, report)
    assert ("r0-research", "crashed") in ended(records) and "r0-research.2" in started(records), records
    path = workspace("misconfigured")
    start = time.time()
    with provider(scripted, scripted_idea) as (url, _):
        result = ar(path, url, ok=False, env={"HX_AR_RETRY_SECS": "0"}, key="HX_AR_NO_SUCH_KEY")
    assert result.returncode != 0 and "keep failing" in result.stderr and time.time() - start < 120, result.stderr[-1500:]
    check("crashes are charged from child journals and retried; misconfiguration fails fast")


def scenario_16():
    # 12. Search and select invoke their configured evaluator commands.
    grader, holdout = held_out_roots()
    spec = (f'editable = ["x.txt"]\neval = "python3 search.py"\nselect = "sh {grader}/score.sh {holdout}"\n'
            f'grader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\n')
    path = workspace("holdout", spec=spec)
    with provider(once('return ar.eval("baseline")')) as (url, _):
        report = report_of(ar(path, url))
    assert all(r["score"] == -49 for r in ledger(path, report)) and report["select"] == 1, (ledger(path, report), report["select"])
    # A child's frozen policy (in its journal, kept with the run) never names the held-out evaluator.
    episodes = ar_runs(path) / report["run"] / "episodes"
    seen = [p for p in episodes.rglob("*") if p.is_file() and not p.name.endswith((".md", ".stderr"))]
    assert seen and not any(b"score.sh" in p.read_bytes() for p in seen), [p.name for p in seen if b"score.sh" in p.read_bytes()]
    check("search and select invoke their configured evaluator commands")


def scenario_17():
    # --yes episodes scrub credentials and file tools still seal owner and grader data.
    grader, holdout = held_out_roots()
    spec = f'editable = ["x.txt"]\neval = "python3 search.py"\ngrader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\n'
    path = workspace("autopilot-seal", spec=spec)
    (ar_runs(path) / "old-run").mkdir(parents=True)
    (ar_runs(path) / "old-run/journal.jsonl").write_text("secret journal")
    look = ('local r = sh("env | grep -c -E \'HX_TEST_KEY|HX_AR_EPISODE\'")\n'
            f'local a, ae = pcall(read, "{ar_runs(path)}/old-run/journal.jsonl")\n'
            f'local b, be = pcall(read, "{holdout}/secret.txt")\n'
            f'local c, ce = pcall(write, "{grader}/score.sh", "x")\n'
            'print(ar.eval("b"))\nreturn "OUT " .. r.stdout .. tostring(ae) .. tostring(be) .. tostring(ce)')
    with provider(once(look)) as (url, requests):
        ar(path, url, extra=["--yes"])
    outs = [o for o in outputs(requests) if "OUT " in o]
    assert outs and all(o.split("OUT ")[1].lstrip().startswith("0") and o.count("sealed owner state") == 3 for o in outs), outs
    assert (grader / "score.sh").read_text().count("\n") == 1, "grader root was modified"
    check("--yes episodes scrub credentials and file tools seal owner and grader data")


def scenario_18():
    # 14. Detached processes of agents and evaluators die with the command or evaluation that started them.
    marker = f"hx-ar-reap-{os.getpid()}"
    spec = f'editable = ["x.txt"]\neval = "python3 search.py; (setsid bash -c \'exec -a {marker}-eval sleep 300\' >/dev/null 2>&1 &)"\ntimeout = 30\n'
    files = {"x.txt": "0\n", "search.py": f'import json, subprocess\nsubprocess.Popen(["bash", "-c", "exec -a {marker}-py sleep 300"], start_new_session=True)\nprint(json.dumps({{"score": 0}}))'}
    path = workspace("reap", files, spec)
    daemon = (f'sh("(setsid bash -c \'exec -a {marker}-agent sleep 300\' >/dev/null 2>&1 &); sleep 0.5")\n'
              f'print("SAW " .. sh("grep -l ^{marker[0]}[{marker[1]}]{marker[2:]} /proc/[0-9]*/cmdline | wc -l").stdout)\nreturn ar.eval("daemon")')
    with provider(once(daemon)) as (url, requests):
        report_of(ar(path, url))
    # The agent's next command saw nothing its previous one left (its namespace died with it).
    assert any("SAW 0" in o for o in outputs(requests)), outputs(requests)
    time.sleep(0.5)
    def running():
        out = []
        for p in Path("/proc").iterdir():
            try:
                if p.name.isdigit() and (p / "cmdline").read_bytes().startswith(marker.encode()): out.append(p.name)
            except OSError:
                pass
        return out
    assert not running(), f"detached processes survived: {running()}"
    check("detached agent and evaluator processes die with the command that started them")


def scenario_19():
    # 15. Snapshot limits: too many files or too deep a tree is refused per eval, never fatal to the run.
    files = {"pkg/a.txt": "0\n", "search.py": 'import json; print(json.dumps({"score": 1}))'}
    path = workspace("limits", files, 'editable = ["pkg"]\neval = "python3 search.py"\ntimeout = 30\n')
    code = ('sh("mkdir -p pkg/many && cd pkg/many && seq 1 10001 | xargs touch")\nlocal ok1, e1 = pcall(ar.eval, "many")\n'
            'sh("rm -rf pkg/many; mkdir -p pkg/$(printf \'d/%.0s\' $(seq 1 40))")\nlocal ok2, e2 = pcall(ar.eval, "deep")\n'
            'sh("rm -rf pkg/d")\nprint(ar.eval("fine"))\nreturn "E1 " .. tostring(e1) .. " E2 " .. tostring(e2)')
    with provider(once(code)) as (url, requests):
        report = report_of(ar(path, url))
    outs = " ".join(outputs(requests))
    assert "more than 10000 files" in outs and "deeper than 32" in outs and report["evals"] >= 1, outs[-2000:]
    check("snapshot file-count and depth limits refuse one eval, not the run")


def scenario_20():
    # 16. The frozen base is the committed tree byte for byte: export attributes do not apply.
    files = {"x.txt": "0\n", ".gitattributes": "hidden.txt export-ignore\nsubst.txt export-subst\n", "hidden.txt": "here\n",
             "subst.txt": "$Format:%H$\n",
             "search.py": 'import json\nok = open("hidden.txt").read() == "here\\n" and open("subst.txt").read() == "$Format:%H$\\n"\nprint(json.dumps({"score": 1 if ok else 0}))'}
    path = workspace("attributes", files, 'editable = ["x.txt"]\neval = "python3 search.py"\ntimeout = 30\n')
    with provider(once('return ar.eval("b")')) as (url, _):
        report = report_of(ar(path, url))
    assert ledger(path, report)[0]["score"] == 1, ledger(path, report)
    check("the frozen base ignores export attributes")


def scenario_21():
    # 16b. Each state is scored once; ar.reset() restores the best state.
    code = ('print("A", ar.eval("baseline").cached)\nwrite("x.txt", "6\\n")\nprint("B", ar.eval("six").improved)\n'
            'write("x.txt", "1\\n")\nprint("C", ar.eval("one").improved)\nprint("R", ar.reset().best)\n'
            'print("X", read("x.txt"))\nlocal r = ar.eval("six again")\nreturn "D " .. tostring(r.cached) .. " " .. r.n')
    path = workspace("reset")
    with provider(once(code, ("r1-w1",))) as (url, requests):
        report = report_of(ar(path, url))
    out = " ".join([o for o in outputs(requests) if "D " in o][0].split())
    assert "A nil" in out and "B true" in out and "C false" in out and "R -1" in out and "X 6" in out and "D true 2" in out, out
    rows = ledger(path, report)
    assert [r["note"] for r in rows[:3]] == ["baseline", "six", "one"] and not any(r["note"] == "six again" for r in rows), rows
    scores = [r for r in journal_of(path, report) if r["kind"] == "score" and r["tier"] == "search"]
    assert len({r["snap"] for r in scores}) == len(scores), "a state was scored twice"
    check("re-scoring a scored state returns the stored result without a new eval, and ar.reset restores the best")


def scenario_22():
    # 16c. ar.reset never writes or deletes through a symlink the agent planted in an editable path's parent.
    outside = BASE / "outside"
    outside.mkdir()
    (outside / "x.txt").write_text("keep\n")
    code = (f'print(ar.eval("baseline"))\nsh("rm -rf pkg && ln -s {outside} pkg")\n'
            'local ok, e = pcall(ar.reset)\nreturn "RESET " .. tostring(ok) .. " " .. tostring(e)')
    path = workspace("reset-link", {"pkg/x.txt": "0\n", "search.py": 'import json; print(json.dumps({"score": 1}))'},
                     'editable = ["pkg/x.txt"]\neval = "python3 search.py"\ntimeout = 30\n')
    with provider(once(code, ("r1-w1",))) as (url, requests):
        ar(path, url)
    out = [o for o in outputs(requests) if "RESET " in o][0]
    assert "RESET false" in out and (outside / "x.txt").read_text() == "keep\n", (out, list(outside.iterdir()))
    check("ar.reset refuses a symlinked parent and leaves the link target untouched")


def scenario_23():
    # 17. Launch validation.
    path = workspace("flags")
    base_cmd = [str(harness.HX), "run", "--ar", "--workspace", str(path), "--prompt-file", str(path / "program.md"), "--model", "mock"]
    result = subprocess.run(base_cmd[:-4] + ["--model", "mock"], env=harness.ENV, capture_output=True, text=True)
    assert result.returncode != 0 and "--prompt-file" in result.stderr, result.stderr
    result = subprocess.run(base_cmd + ["--max-turns", "5"], env=harness.ENV, capture_output=True, text=True)
    assert result.returncode != 0 and "--max-turns cannot be combined with --ar" in result.stderr, result.stderr
    (path / "notes.txt").write_text("uncommitted")
    result = subprocess.run(base_cmd, env=harness.ENV, capture_output=True, text=True)
    assert result.returncode != 0 and "notes.txt" in result.stderr, result.stderr
    (path / "notes.txt").unlink()
    (path / ".harness/ar.toml").write_text('editable = ["."]\neval = "true"\n')
    result = subprocess.run(base_cmd, env=harness.ENV, capture_output=True, text=True)
    assert result.returncode != 0 and "editable path" in result.stderr, result.stderr
    check("launch requires a program, a clean scope and sane editable paths; episodes are limited in turns, not by flags")


def append_record(journal, kind, **fields):
    """Append a record to a run's journal the way hx writes one, hash chain and all."""
    last = journal.read_text().splitlines()[-1]
    record = {"seq": json.loads(last)["seq"] + 1, "ts_ms": 0, "session": "journal", "kind": kind,
              "prev": hashlib.sha256(last.encode()).hexdigest(), **fields}
    with open(journal, "a") as f:
        f.write(json.dumps(record, separators=(",", ":")) + "\n")


def scenario_24():
    # 7d. Resume takes only this workspace's runs in the state directory (a run kept in the workspace, where shell
    # commands could rewrite it, is refused), a live run cannot be resumed twice, a run of an older format is refused,
    # and an episode the journal names as open is cleaned up only where hx puts its copies, never through a planted link.
    path = workspace("tamper")
    with provider(lambda body, index: hopeless(body, index, "nope")) as (url, _):
        report = report_of(ar(path, url))
        run = ar_runs(path) / report["run"]
        (run / "report.json").unlink()
        resume = [str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)]
        old = path / ".harness/ar" / report["run"]
        shutil.copytree(run, old)
        (old / "journal.jsonl").write_text((old / "journal.jsonl").read_text().replace("python3 search.py", "touch TAMPERED; python3 search.py"))
        result = subprocess.run(resume[:4] + [str(old)] + resume[5:], env=harness.ENV, capture_output=True, text=True, timeout=600)
        assert result.returncode != 0 and "predates that and cannot be resumed" in result.stderr, result.stderr[-1500:]
        listed = subprocess.run([str(harness.HX), "runs", "--workspace", str(path)], env=harness.ENV, capture_output=True, text=True)
        assert f"{run}  autoresearch, unfinished" in listed.stdout, listed.stdout

        holder = subprocess.Popen(["python3", "-c", "import fcntl, sys, time; f = open(sys.argv[1], 'w'); "
                                   "fcntl.flock(f, fcntl.LOCK_EX); print('held', flush=True); time.sleep(60)", str(run / "journal.lock")],
                                  stdout=subprocess.PIPE, text=True)
        try:
            assert holder.stdout.readline().strip() == "held"
            result = subprocess.run(resume, env=harness.ENV, capture_output=True, text=True, timeout=600)
            assert result.returncode != 0 and "in use by another hx" in result.stderr, result.stderr[-1500:]
        finally:
            holder.kill()
            holder.wait()

        older = ar_runs(path) / "1-1.noindex"
        older.mkdir()
        (older / "launch.json").write_text("{}")
        (older / "journal.jsonl").write_text('{"kind": "planned"}\n')
        result = subprocess.run(resume[:4] + [str(older)] + resume[5:], env=harness.ENV, capture_output=True, text=True, timeout=60)
        assert result.returncode != 0 and "started by an older hx" in result.stderr, result.stderr[-1500:]

        victim = BASE / "tamper-victim"
        victim.mkdir()
        (victim / "keep.txt").write_text("keep")
        planted = Path(tempfile.gettempdir()) / f"hx-ar-ws-1-{run.name.removesuffix('.noindex')}-r1-w9.noindex"
        planted.symlink_to(victim)
        append_record(run / "journal.jsonl", "episode", plan="r1-w9", attempt=1, role="explore", outcome="started")
        # By run id, as `hx runs` shows it.
        result = subprocess.run(resume[:4] + [report["run"]] + resume[5:], env=harness.ENV, capture_output=True, text=True, timeout=600)
        assert result.returncode == 0, result.stderr[-1500:]
        assert (victim / "keep.txt").read_text() == "keep" and planted.is_symlink(), result.stderr[-1500:]
        assert ("r1-w9", "interrupted") in ended(records_of(run))
        planted.unlink()
        assert not (path / "TAMPERED").exists()
    check("resume refuses a run kept in the workspace, a run in use and an older run, and never deletes through a planted copy")


def scenario_fallback():
    # A refinement whose state fails its audit costs only itself: the candidate falls back to its best state that passed.
    def strict(body, index):
        if role_of(body)[0] == "audit" and '"x": 7' in text_of(body).split("<starting_state>")[1].split("</starting_state>")[0]:
            return audit(body, False, "x is seven")
        return scripted(body, index)

    path = workspace("fallback")
    with provider(strict, scripted_idea) as (url, _):
        report = report_of(ar(path, url))
    run = ar_runs(path) / report["run"]
    records = records_of(run)
    six = next(p["candidate"] for p in plans(records, "round1") if p["idea"] == go(6))
    of = {r["snap"]: r["candidate"] for r in evals(records)}
    assert any(r["kind"] == "verdict" and of.get(r["snap"]) == six and not r["pass"] for r in records), "the x = 7 state was never failed"
    kept = next(p for p in report["portfolio"] if p["candidate"] == six)
    snap = snapshot(run, kept["snap"])
    assert snap["x.txt"] == b"6\n" and any(r["kind"] == "verdict" and r["snap"] == kept["snap"] and r["pass"] for r in records), kept
    check("a refinement that fails its audit costs only itself: the candidate keeps its best state that passed")


# Review findings: the gate, interrupts, confinement, the watchdog, --finish and the binary episodes run. These use the
# linear task (search = x) and few workers, so each takes seconds.
LINEAR = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": %s, "report": {"x": x}}))'
CONST = 'import json; print(json.dumps({"score": 0}))'


def lanes(steps, verdicts=lambda x: True, other=None):
    """A provider reply whose workers follow `steps` ({plan label: [x, ...]}: score each x in turn) and whose auditors
    judge the x of the state they audit (`verdicts(x)`: True, False, or None for no verdict). Episodes without a lane go
    to `other` if given, else finish at once."""
    def reply(body, index):
        role, label = role_of(body)
        if role == "audit":
            x = int(re.search(r'"x": (-?\d+)', text_of(body).split("<starting_state>")[1]).group(1))
            v = verdicts(x)
            return audit(body, v, "" if v else f"x = {x} breaks the rules")
        steps_ = steps.get((label or "").split(".")[0])
        if steps_ is None: return other(body, index) if other else ("BRIEF" if role == "research" else "done")
        if answered(body): return f"FINDING from {label}"
        return {"code": "".join(f'write("x.txt", "{x}\\n")\nprint(ar.eval("x={x}"))\n' for x in steps_) + 'return "ok"'}
    return reply


def scenario_veto():
    # Finding 1: a refinement the held-out veto rejects costs only itself; the candidate keeps its passed result, its
    # next workers are told why, and max_rounds ends the run.
    path = workspace("veto", {"x.txt": "0\n", "search.py": LINEAR % "x", "select.py": SCORE % 5}, workers(2, 3))
    with provider(lanes({"r1-w1": [3], "r2-w1": [11], "r2-w2": [4]})) as (url, requests):
        result = ar(path, url)
    report, records = report_of(result), journal_of(path)
    rows = evals(records)
    x11 = next(r["snap"] for r in rows if r["note"] == "x=11")
    assert [s for snap, s in selects(records).items() if snap == x11] == [-36], records
    gates = [r for r in records if r["kind"] == "gate"]
    assert all(c["frontier"]["snap"] != x11 for c in report["candidates"]), report["candidates"]
    assert not any(p["start"] == x11 for p in plans(records))
    task = (ar_runs(path) / report["run"] / "episodes/r3-w1.md").read_text()
    assert "was vetoed" in task and "-36" not in task, task.split("<checks>")[-1][:500]
    assert [snap_x(path, report, p["snap"]) for p in report["portfolio"]] == [4] and (path / "x.txt").read_text() == "4\n", report["portfolio"]
    assert "3 rounds, the most this run takes" in result.stderr and gates[-1]["last"], result.stderr[-800:]
    check("a vetoed refinement falls back like a failed audit: the candidate keeps a result and its workers learn why (finding 1)")


def scenario_noverdict_keeps_result():
    # Finding 1: an audit that ends without a verdict leaves the candidate's earlier passed result in place.
    path = workspace("noverdict2", {"x.txt": "0\n", "search.py": LINEAR % "x", "select.py": LINEAR % "x"}, workers(2, 2))
    with provider(lanes({"r1-w1": [3], "r2-w1": [4]}, lambda x: True if x == 3 else None)) as (url, _):
        result = ar(path, url)
    report, records = report_of(result), journal_of(path)
    assert any(r["kind"] == "planned" and r["batch"] == "audit-last" for r in records), "the lift was never audited"
    assert [snap_x(path, report, p["snap"]) for p in report["portfolio"]] == [3] and (path / "x.txt").read_text() == "3\n", report
    assert "no verdict; it stays unaudited" in result.stderr
    check("an audit without a verdict keeps the candidate's passed result (finding 1)")


def scenario_fallback_derived():
    # Finding 8 and 7: after a failed audit the candidate falls back to its best surviving state (5, not 3), whether or
    # not that state was `improved` when scored, and the exploit task names the failed audit's reason.
    path = workspace("fallback2", {"x.txt": "0\n", "search.py": LINEAR % "x", "select.py": CONST}, workers(1, 2))
    with provider(lanes({"r1-w1": [3, 6, 5]}, lambda x: x != 6)) as (url, _):
        result = ar(path, url)
    report, records = report_of(result), journal_of(path)
    assert snap_x(path, report, plans(records, "round2")[0]["start"]) == 5, plans(records, "round2")
    task = (ar_runs(path) / report["run"] / "episodes/r2-w1.md").read_text()
    assert "failed its audit: x = 6 breaks the rules" in task, task[:2000]
    assert [snap_x(path, report, p["snap"]) for p in report["portfolio"]] == [5] and (path / "x.txt").read_text() == "5\n", report
    check("a failed audit falls back to the best surviving state, and its reason reaches the candidate's workers (findings 7, 8)")


def scenario_tie():
    # Finding 9: a state that ties the seed's score is a candidate like any other: not good, ranked by search.
    path = workspace("tie", {"x.txt": "0\n", "search.py": LINEAR % "x // 10", "select.py": CONST}, workers(2, 2))
    with provider(lanes({"r1-w1": [5], "r1-w2": [-5]})) as (url, _):
        result = ar(path, url)
    report, records = report_of(result), journal_of(path)
    gate = next(r for r in records if r["kind"] == "gate")
    # Round 2 builds nothing, so the last gate's decisions are the first gate's.
    ties = [c for c in report["candidates"] if c["frontier"]["search"] == 0]
    assert len(ties) == 1 and not ties[0]["good"] and gate["chosen"][0] == ties[0]["candidate"] and len(gate["chosen"]) == 2, (gate, report["candidates"])
    assert {p["candidate"] for p in plans(records, "round2")} == set(gate["chosen"]), plans(records, "round2")
    check("a tie with the seed is not good and ranks by search; only the seed's own bytes are excluded (finding 9)")


def scenario_interrupt():
    # Finding 2: stopping the orchestrator three times before round 1's workers score uses up nothing; once the provider
    # works, the run finishes. Each killed child's temp directory was inside its scratch copy and went with it (15).
    path = workspace("interrupt", spec=workers(2, 1))
    working = lanes({"r1-w1": [3], "r1-w2": [2]})
    fixed = []  # the provider works once the owner has fixed it; the run keeps the provider it was launched with
    def sleepy(body, index):
        role = role_of(body)[0]
        if fixed: return working(body, index)
        if role == "explore" and not answered(body): return {"code": 'sh("sleep 120")\nreturn "late"'}
        return "BRIEF" if role == "research" else "done"
    def wait_started(n):
        deadline = time.time() + 120
        while time.time() < deadline:
            js = list(ar_runs(path).glob("*/journal.jsonl"))
            if js and sum(l.startswith("r1-") for l in started(records_of(js[0].parent))) >= n:
                return js[0].parent
            time.sleep(0.1)
        raise AssertionError(f"round 1 never started {n} attempts")
    resume = lambda run: [str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)]
    with provider(sleepy) as (url, _):
        p = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        for n in (2, 4, 6):
            run = wait_started(n)
            time.sleep(1)
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
            if n < 6:
                p = subprocess.Popen(resume(run), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        fixed.append(True)
        result = subprocess.run(resume(run), env=harness.ENV, capture_output=True, text=True, timeout=300)
    assert result.returncode == 0, result.stderr[-2000:]
    records = journal_of(path)
    assert [l for l in started(records) if l.startswith("r1-w1")] == ["r1-w1", "r1-w1.2", "r1-w1.3", "r1-w1.4"]
    killed = [l for l, o in ended(records) if o == "interrupted"]
    assert len(killed) == 6 and [g["chosen"] for g in records if g["kind"] == "gate"][0], records
    for l in killed:
        session = "".join(j.read_text() for j in (run / "episodes" / f"{l}.log").glob("*.jsonl"))
        tmp = re.search(r'"tmp":"([^"]+)"', session).group(1)
        copy = re.match(rf".*/hx-ar-ws-\d+-{re.escape(run.name.removesuffix('.noindex'))}-{re.escape(l)}\.noindex", tmp)
        assert copy and tmp.startswith(copy.group(0) + "/tmp/") and not Path(tmp).exists() and not Path(copy.group(0)).exists(), tmp
    check("interrupted attempts are not crashes: a run stopped three times mid-round still finishes, and killed children leave no temp directory (findings 2, 15)")


def scenario_unconfined():
    # Finding 3: without Landlock a spec with held-out or grader data refuses to launch or resume, unless acknowledged.
    grader, holdout = held_out_roots()
    spec = (f'editable = ["x.txt"]\neval = "python3 search.py"\nselect = "sh {grader}/score.sh {holdout}"\n'
            f'grader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\nworkers = 1\nmax_rounds = 1\n')
    path = workspace("unconfined", spec=spec)
    off = {"HX_TEST_NO_LANDLOCK": "1"}
    with provider(once('return ar.eval("b")', ("r1-w1",))) as (url, requests):
        refused = ar(path, url, ok=False, env=off)
        assert refused.returncode != 0 and "--ar-unconfined" in refused.stderr and not requests, refused.stderr[-800:]
        assert not list(ar_runs(path).glob("*")), "a refused launch left a run"
        report = report_of(ar(path, url, env=off, extra=["--ar-unconfined"]))
        run = ar_runs(path) / report["run"]
        (run / "report.json").unlink()
        before = len(requests)
        resume = [str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)]
        result = subprocess.run(resume, env={**harness.ENV, **off}, capture_output=True, text=True, timeout=120)
        assert result.returncode != 0 and "--ar-unconfined" in result.stderr, result.stderr[-800:]
        result = subprocess.run(resume + ["--ar-unconfined"], env={**harness.ENV, **off}, capture_output=True, text=True, timeout=120)
        assert result.returncode == 0 and len(requests) == before, result.stderr[-800:]
    check("without confinement, held-out data refuses launch and resume unless --ar-unconfined acknowledges it (finding 3)")


def scenario_watchdog():
    # Finding 11: a child that outlives its wall-clock cap is killed and journaled as timed out, an outcome, not a crash.
    path = workspace("watchdog", spec='editable = ["x.txt"]\neval = "python3 search.py"\ntimeout = 30\nworkers = 1\nmax_rounds = 1\n')
    def stuck(body, index):
        if role_of(body)[0] == "research" and not answered(body):
            time.sleep(20)
        return lanes({"r1-w1": [3]})(body, index)
    with provider(stuck) as (url, _):
        result = ar(path, url, env={"HX_AR_WATCHDOG_SECS": "4"})
    research = [e for e in ended(journal_of(path)) if e[0].startswith("r0-research")]
    assert research == [("r0-research", "timed_out")], research
    assert "outlived its" in result.stderr and report_of(result)["evals"] >= 1, result.stderr[-800:]
    check("the watchdog kills a child past its cap and journals it as timed out, not crashed (finding 11)")


def scenario_finish():
    # Finding 14: `--resume RUN --finish` writes the result from the last gate and starts nothing.
    path = workspace("finish", {"x.txt": "0\n", "search.py": LINEAR % "x", "select.py": LINEAR % "x"}, workers(2))
    slow = lambda body, index: {"code": 'sh("sleep 60")\nreturn "late"'} if role_of(body)[0] == "exploit" and not answered(body) else "done"
    with provider(lanes({"r1-w1": [3]}, other=slow)) as (url, requests):
        p = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        deadline = time.time() + 120
        while time.time() < deadline and "r2-w1" not in (started(journal_of(path)) if list(ar_runs(path).glob("*/journal.jsonl")) else []):
            time.sleep(0.1)
        time.sleep(1)
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()
        before = len(requests)
        run = sorted(ar_runs(path).glob("*/journal.jsonl"))[-1].parent
        resume = [str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)]
        result = subprocess.run(resume + ["--finish"], env=harness.ENV, capture_output=True, text=True, timeout=120)
        assert result.returncode == 0 and len(requests) == before, result.stderr[-1500:]
        report = report_of(result)
        assert [snap_x(path, report, p["snap"]) for p in report["portfolio"]] == [3] and (path / "x.txt").read_text() == "3\n", report
        assert (run / "report.json").exists() and (run / "portfolio").is_dir()
        again = subprocess.run(resume, env=harness.ENV, capture_output=True, text=True, timeout=120)
        assert again.returncode != 0 and "already finished" in again.stderr, again.stderr[-800:]
    check("--resume RUN --finish writes the last gate's result and starts nothing (finding 14)")


def scenario_bad_seed():
    # Finding 5: a seed the search evaluator cannot score stops the run before spending, and nothing touches the workspace.
    path = workspace("badseed", {"x.txt": "zero\n", "search.py": SCORE % 7, "select.py": SCORE % 5})
    with provider(scripted, scripted_idea) as (url, requests):
        result = ar(path, url, ok=False)
    assert result.returncode != 0 and "no valid score for the seed" in result.stderr and not requests, result.stderr[-800:]
    status = subprocess.run(["git", "status", "--porcelain"], cwd=path, capture_output=True, text=True).stdout
    assert not status.strip() and (path / "x.txt").read_text() == "zero\n", status
    check("a seed the evaluator cannot score fails fast; the workspace is untouched (finding 5)")


def scenario_rebuild():
    # Finding 15: episodes run the binary the orchestrator started as, even once the file at its path is gone (a rebuild
    # replaces it); an episode's shell gets a temp directory inside its scratch copy.
    copy = BASE / "hx-rebuilt"
    shutil.copy2(os.environ.get("HX_BIN") or harness.ROOT / "target/release/hx", copy)
    path = workspace("rebuild", spec='editable = ["x.txt"]\neval = "python3 search.py"\ntimeout = 30\nworkers = 1\nmax_rounds = 1\n')
    def reply(body, index):
        if role_of(body)[0] == "research" and not answered(body):
            copy.unlink(missing_ok=True)
            return {"code": 'return "TMP " .. sh("echo $TMPDIR").stdout'}
        return lanes({"r1-w1": [3]})(body, index)
    with provider(reply) as (url, requests):
        result = ar(path, url, env={"HX_BIN": str(copy)})
    assert not copy.exists() and report_of(result)["evals"] >= 1, result.stderr[-1500:]
    tmp = next(o for o in outputs(requests) if "TMP " in o).split("TMP ")[1].split()[0]
    assert re.search(r"/hx-ar-ws-[^/]+/tmp/hx-\d+-\d+$", tmp), tmp
    check("episodes keep running the orchestrator's own binary after it is replaced, with temp directories in their copies (finding 15)")


def scenario_late_verdict():
    # An auditor that works until the turn-cap notice and records its verdict in the turn after it (the last one with
    # tools) is audited; its tool-less last reply is prose, which never counts.
    path = workspace("late-verdict", {"x.txt": "0\n", "search.py": LINEAR % "x"}, 'editable = ["x.txt"]\neval = "python3 search.py"\ntimeout = 30\nworkers = 1\nmax_rounds = 1\n')
    turns = []
    def reply(body, index):
        if role_of(body)[0] != "audit": return lanes({"r1-w1": [3]})(body, index)
        outs = outputs([body])
        turns.append(len(outs))
        if any("RECORDED" in o for o in outs): return audit(body, True)
        if outs and "[hx] The run stops" in outs[-1]: return verdict_code(True)
        return {"code": 'return "still looking"'}
    with provider(reply) as (url, _):
        report = report_of(ar(path, url))
    records = journal_of(path)
    assert max(turns) >= 30 and [r["pass"] for r in records if r["kind"] == "verdict"] == [True], (max(turns), records[-3:])
    assert [snap_x(path, report, p["snap"]) for p in report["portfolio"]] == [3], report
    check("an auditor that records its verdict right after the turn-cap notice is audited")


# Scenarios are independent (own workspace, own mock provider), so a few run side by side; more would load the machine.
with ThreadPoolExecutor(max_workers=4) as pool:
    only = os.environ.get("HX_CHECK_FILTER", "")
    for future in [pool.submit(f) for f in [v for k, v in sorted(globals().items()) if k.startswith("scenario_") and only in k]]:
        future.result()
print(f"{passed}/{passed} ar checks passed")
