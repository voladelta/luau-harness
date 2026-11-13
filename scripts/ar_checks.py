#!/usr/bin/env python3
"""Offline autoresearch (`hx run --ar`) checks with the real CLI, a scripted provider and a toy evaluator.

The toy task: x.txt holds an integer; the search evaluator scores -(x-7)^2 and the held-out one -(x-5)^2. Candidates
rank by search; the held-out score only vetoes those worse than the seed on it (x of 10 and beyond). Scripted runs exercise the manager loop:
research, first-come claims judged by the model, gates with audits, exploit rounds and the portfolio. The isolation
checks replay attacks an independent review found against the first design: each must leave the recorded scores honest.
"""
import base64
import json
import os
import re
import shutil
import signal
import subprocess
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


def ledger(path, report):
    return [json.loads(line) for line in (ar_runs(path) / report["run"] / "ledger.jsonl").read_text().splitlines()]


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
    if "You compare research ideas" in t: return "judge", None
    for role, marker in (("research", "## Autoresearch research lead ("), ("explore", "## Autoresearch explore worker "),
                         ("exploit", "## Autoresearch exploit worker "), ("audit", "## Autoresearch auditor ")):
        if marker in t:
            return role, t.split(marker, 1)[1].split(None, 1)[0].rstrip(":)")
    return "other", None


def judge_same(body):
    """A judge that finds two claims the same core when their text is identical."""
    t = text_of(body)
    listed = re.findall(r"^(\d+)\. (.*)$", t.split("New claim:")[0].split("Claimed so far:")[1], re.M)
    new = t.split("New claim:\n", 1)[1].split("\n", 1)[0].strip()
    same = "none" if "probe sentence" in new else next((n for n, idea in listed if idea.strip() == new), "0")
    return f"The new claim's mechanism is {new}.\nAnswer: {same}"


def once(code, where=("research",)):
    """A provider whose agent runs `code` once in the named roles or episode labels and finishes; every other episode
    finishes at once. Code that runs in a worker first claims an idea."""
    def reply(body, index):
        role, label = role_of(body)
        if role == "judge": return "Answer: 0"
        if answered(body) or (role not in where and label not in where): return "done"
        return {"code": (f'pcall(ar.claim, "a probe idea from worker {label} used only by the isolation checks")\n' if role in ("explore", "exploit") else "") + code}
    return reply


def outputs(requests):
    return [item.get("output", "") for body in requests for item in body.get("input", []) if item.get("type") == "function_call_output"]


def verdict_code(passed, reason=""):
    return {"code": f'if ar.verdict then ar.verdict{{pass = {str(passed).lower()}, reason = {json.dumps(reason)}}}; return "RECORDED" end\n'
                    'return "PWD " .. sh("pwd").stdout'}


def audit(body, passed, reason=""):
    """An auditor's reply: its verdict through `ar.verdict{pass, reason}` (passed=None: it gives none). An hx whose
    episodes do not have `ar.verdict` yet gets the verdict file `ar.verdict` writes from the check itself, found through
    the auditor's working directory (its scratch copy, journaled when the episode started)."""
    if passed is None: return "Checked the state; ran out of steps before deciding."
    if not answered(body): return verdict_code(passed, reason)
    pwd = next((o.split("PWD ", 1)[1].split()[0] for o in outputs([body]) if "PWD " in o), None)
    if pwd:
        label = role_of(body)[1]
        for journal in (harness.BASE / "xdg-state").rglob("ar/*/journal.jsonl"):
            lines = [json.loads(l) for l in journal.read_text().splitlines() if l.endswith("}")]
            if any(r["kind"] == "started" and r["label"] == label and r.get("scratch") and pwd.startswith(r["scratch"] + "/") for r in lines):
                path = journal.parent / "episodes" / f"{label}.verdict.json"
                (path.parent / f".{label}.tmp").write_text(json.dumps({"pass": passed, "reason": reason}))
                (path.parent / f".{label}.tmp").rename(path)
    return "Checked the state."


def journal_of(path, report=None):
    run = ar_runs(path) / report["run"] if report else sorted(ar_runs(path).glob("*/journal.jsonl"))[-1].parent
    return [json.loads(l) for l in (run / "journal.jsonl").read_text().splitlines() if l.endswith("}")]


def snap_x(path, report, snap):
    """x of a snapshot of the toy task."""
    return int(base64.b64decode(json.loads((ar_runs(path) / report["run"] / "snaps" / f"{snap}.json").read_text())["x.txt"]))


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

# 1. The manager loop: research, explore workers claiming first come first served, a gate with an audit, exploit
# rounds with distinct refinements, and a portfolio of distinct audited candidates ranked by held-out score.
TARGETS = {1: 3, 2: 6, 3: 20, 4: 6, 5: 9, 6: 1, 7: 12, 8: 13}  # workers 2 and 4 want the same idea: one of them must be turned away
UP, DOWN = "step up by adding one to the current value of x", "step down by subtracting one from the current value of x"
def go(x):
    """A claim's text: at least eight words, as ar.claim requires."""
    return f"go to {x} by writing that one value into x.txt"


def scripted(body, index):
    role, label = role_of(body)
    if role == "judge":
        return judge_same(body)
    if role == "audit":
        return audit(body, False, "probe trick") if f"({go(3)})" in text_of(body) else audit(body, True)
    if answered(body):
        return f"FINDING from {label}: done"
    if role == "research":
        return "BRIEF: the score rewards x close to the hidden target; avoid huge x."
    if role == "explore":
        w = int(label.split("-w")[1].split(".")[0])
        t, alt = TARGETS[w], 40 + w
        # A retry gets its earlier attempt's claim back from its first ar.claim call, so only first attempts probe.
        placeholder = 'print("PLACEHOLDER", ar.claim("a short probe sentence that is long enough to be accepted here").message)\n' if "." not in label else ""
        return {"code": f'print("PROBE", pcall(ar.claim, "x"))\nprint("CLAIMS", ar.claims().message)\n{placeholder}'
                        f'local r = ar.claim("{go(t)}")\nlocal x = {t}\nif not r.ok then r = ar.claim("{go(alt)}"); x = {alt} end\n'
                        f'print("CLAIM", r.ok, r.n)\nwrite("x.txt", tostring(x) .. "\\n")\nprint(ar.eval("go " .. x))\n'
                        f'write("x.txt", "{100 + index}\\n")\nprint(ar.eval("probe"))\nar.reset()\nreturn "RESET " .. x .. " " .. read("x.txt")'}
    if role == "exploit":
        return {"code": f'local x = tonumber(read("x.txt"))\nlocal r = ar.claim("{UP}")\nlocal d = 1\n'
                        f'if not r.ok then r = ar.claim("{DOWN}"); d = -1 end\n'
                        'if r.ok then write("x.txt", tostring(x + d) .. "\\n"); print(ar.eval("step " .. d)) end\nreturn "STEP " .. d'}
    return "done"


# 1b. While refining keeps making progress, rounds go on; the score tops out at 12, and then the run stops by itself.
CLIMB = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": min(x, 12)}))'
def climbing(body, index):
    role, label = role_of(body)
    if role == "judge": return judge_same(body)
    if role == "audit": return audit(body, True)
    if answered(body): return f"FINDING from {label}: done"
    if role == "explore":
        # Three ideas work (more would keep the run climbing until its round cap); the others build nothing valid.
        w = int(label.split("-w")[1].split(".")[0])
        x = w + 6 if w <= 3 else "nope"
        return {"code": f'ar.claim("{go(w)}")\nwrite("x.txt", "{x}\\n")\nreturn ar.eval("go {w}")'}
    if role == "exploit":
        return {"code": f'ar.claim("raise x by one more step, the refinement tried by worker {label}")\n'
                        'write("x.txt", tostring(tonumber(read("x.txt")) + 1) .. "\\n")\nreturn ar.eval("up")'}
    return "done"


# 2. Ideas below the seed still go on to refinement, but never become results: the workspace keeps the seed. With no
# valid idea at all the run stops after round 1.
def hopeless(body, index, x=None):
    role, label = role_of(body)
    if role == "judge": return judge_same(body)
    if answered(body) or role != "explore": return "done"
    w = int(label.split("-w")[1].split(".")[0])
    value = x or ("00" if w == 1 else 30 + w)  # worker 1 rebuilds the seed in other bytes
    return {"code": f'ar.claim("{go(30 + w)}")\nwrite("x.txt", "{value}\\n")\nreturn ar.eval("far")'}


# 2c. An auditor that gives no verdict leaves its candidate unaudited: nothing becomes a result without a passing audit.

# 3. A select evaluator that cannot score the seed stops the run before any episode.

# 2. Tampering outside the editable paths never reaches the evaluator: no Git hooks, no ignore rules, no evaluator edits.

# 3. Editable code running inside the evaluator gets no inherited credentials.

# 4. Agent file tools cannot read sealed owner state, even with --yes.

# 5. A late writer cannot change what is scored: the evaluator reads a private copy of the snapshot.

# 6. Binary files and symlinks in an editable directory are carried or skipped, never fatal.

# 7. A killed run resumes from its journal: the interrupted worker is charged from its own journal and retried, the rest
# of the plan continues.
def slow(body, index):
    role, label = role_of(body)
    if role == "explore" and label == "r1-w6" and not answered(body):
        return {"code": f'print(ar.claim("{go(1)}"))\nsh("sleep 20")\nreturn "late"'}
    return scripted(body, index)


# 7b. A run killed during an audit re-runs that audit on resume; no candidate passes unaudited.
def slow_audit(body, index):
    role, label = role_of(body)
    if role == "audit" and label.startswith("a1-") and not answered(body) and AUDIT_GATE.exists() is False:
        AUDIT_GATE.write_text("")
        return {"code": 'sh("sleep 20")\nreturn "late"'}
    return scripted(body, index)


# 7c. Stopping the orchestrator stops everything it started: episode children and the commands they run.

# 8. Agents that never score anything end the run after round 1, and a run with no evals fails.

# 9. Provider crashes are charged from the child's journal and retried; misconfiguration fails fast and nonzero.
agent_calls = [0]
def limited(body, index):
    # Unavailable twice in a row exhausts the one retry, so the research child crashes (rate limits are waited out instead).
    if role_of(body)[0] == "research" and not answered(body):
        agent_calls[0] += 1
        if agent_calls[0] < 3: return {"status": 503, "error": "unavailable"}
    return once('return ar.eval("b")')(body, index)


# 12. Search and select invoke their configured evaluator commands.

# 13. With --yes an episode still scrubs credentials and file tools seal owner data.

# 14. Detached processes of agents and evaluators are reaped when their scratch copy is deleted.

# 15. Snapshot limits: too many files or too deep a tree is refused per eval, never fatal to the run.

# 16. The frozen base is the committed tree byte for byte: export attributes do not apply.

# 16b. A deterministic evaluator scores each state once; ar.reset() restores the best state.

# 16c. ar.reset never writes or deletes through a symlink the agent planted in an editable path's parent.

# 17. Launch validation.


def scenario_00():
    # 1. The manager loop: research, explore workers claiming first come first served, a gate with an audit, exploit
    # rounds with distinct refinements, and a portfolio of distinct audited candidates ranked by held-out score.
    path = workspace("rounds")
    with harness.provider(scripted) as (url, requests):
        result = ar(path, url)
    report = report_of(result)
    run = ar_runs(path) / report["run"]
    rows = ledger(path, report)
    claims = [json.loads(l) for l in (run / "claims.jsonl").read_text().splitlines()]
    records = [json.loads(l) for l in (run / "journal.jsonl").read_text().splitlines()]
    explore = [c for c in claims if c["scope"] == "explore" and c["worker"].startswith("r1-")]
    assert [c["n"] for c in explore] == list(range(1, 9)), explore
    ideas = [c["idea"] for c in explore]
    assert ideas.count(go(6)) == 1 and len(set(ideas)) == 8, ideas
    loser = 2 if next(c for c in explore if c["idea"] == go(6))["worker"] == "r1-w4" else 4
    assert go(40 + loser) in ideas, (loser, ideas)
    probes = [o for o in outputs(requests) if "PROBE" in o]
    assert len(probes) >= 8 and all("false" in p and "at least 8 words" in p for p in probes), probes[:2]
    placeholders = [o for o in outputs(requests) if "PLACEHOLDER" in o]
    assert len(placeholders) >= 8 and all("names no concrete" in o.split("PLACEHOLDER")[1] for o in placeholders), placeholders[:1]
    assert not any("probe sentence" in c["idea"] for c in claims)
    assert all("CLAIMS\tNo claims yet." in o or "CLAIMS\tClaimed so far:" in o for o in outputs(requests) if "CLAIMS" in o)
    check("claims are served first come, first served: a duplicate idea is turned away and its worker claims another; a placeholder claim is refused")

    assert (run / "episodes/r0-research.report.json").exists() and "BRIEF:" in (run / "episodes/r1-w1.md").read_text()
    gates = [r for r in records if r["kind"] == "gate" and not r["last"]]
    assert [g["round"] for g in gates] == [1, 2, 3, 4], gates
    three = next(c["n"] for c in explore if c["idea"] == go(3))
    six = next(c["n"] for c in explore if c["idea"] == go(6))
    assert gates[0]["chosen"][0] == six and three not in gates[0]["chosen"] and len(gates[0]["chosen"]) == 3, gates[0]
    assert any(r["kind"] == "audited" and r["idea"] == three and not r["pass"] for r in records)
    assert not any(c["idea"] == three for g in gates for c in g["candidates"] if c["idea"] in g["chosen"])
    bad = {c["n"] for c in explore if c["idea"] in (go(20), go(40 + loser), go(12), go(13))}
    selected = {r["snap"]: r["score"] for r in records if r["kind"] == "selected"}
    vetoed = [r for r in rows if r["idea"] in bad and r["valid"] and r["score"] > -49]
    assert vetoed and all(selected[r["snap"]] < -25 for r in vetoed), vetoed  # beat the seed on search, lose on held-out
    assert all(not c["good"] and c["frontier"]["snap"] not in {r["snap"] for r in vetoed}
               for c in gates[0]["candidates"] if c["idea"] in bad), gates[0]["candidates"]
    check("research briefs every worker; the gate keeps good candidates, audits them and drops the one that fails")

    for g in gates[:2]:
        for idea in g["chosen"]:
            refinements = [c["idea"] for c in claims if c["scope"] == f"c{idea}"]
            assert len(refinements) == len(set(refinements)) and set(refinements) <= {UP, DOWN}, (idea, refinements)
    assert any(c["scope"] != "explore" for c in claims)
    judged = [text_of(b) for b in requests if role_of(b)[0] == "judge"]
    assert any("same refinement" in j for j in judged) and any("same core approach" in j for j in judged), len(judged)
    check("exploit rounds split workers across the chosen candidates, each claiming a distinct refinement")

    # Round 3 finds no refinement left, so round 4 explores again; its workers see every earlier idea and claim new ones.
    # Round 4's new ideas don't move the chosen candidates either, so the run stops.
    roles = {r["batch"]: r["episodes"][0]["role"] for r in records if r["kind"] == "planned" and r["batch"].startswith("round")}
    assert roles == {"round1": "explore", "round2": "exploit", "round3": "exploit", "round4": "explore"}, roles
    everything = [c["idea"] for c in claims if c["scope"] == "explore"]
    assert len(everything) > 8 and len(set(everything)) == len(everything), everything
    r4 = (run / "episodes/r4-w1.md").read_text().split("Earlier rounds already claimed these ideas")[1].split("\n2. ")[0]
    assert all(f"#{c['n']}:" in r4 for c in explore) and not any(f"#{c['n']}:" in r4 for c in claims if c["worker"].startswith("r4-")), r4
    assert "Earlier rounds" not in (run / "episodes/r1-w1.md").read_text()
    assert "two rounds in a row made no progress" in result.stderr, result.stderr[-1500:]
    check("a refining round that moves no chosen candidate is followed by an explore round; two rounds without progress stop the run")

    # Replaying the finished run from its journal reaches the same stop without starting anything.
    (run / "report.json").unlink()
    before = (run / "journal.jsonl").read_text()
    with harness.provider(scripted) as (url, replayed):
        replay = subprocess.run([str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)],
                                env=harness.ENV, capture_output=True, text=True, timeout=600)
    assert replay.returncode == 0 and not replayed, replay.stderr[-2000:]
    assert (run / "journal.jsonl").read_text() == before, "the replay journaled something"
    again = report_of(replay)
    assert (again["rounds"], again["snap"], again["portfolio"]) == (report["rounds"], report["snap"], report["portfolio"]), again
    check("a resumed run replays its rounds from the journal: same decisions, nothing re-planned or re-run")

    portfolio = report["portfolio"]
    assert 1 <= len(portfolio) <= 3 and len({p["snap"] for p in portfolio}) == len(portfolio), portfolio
    passed = {r["snap"] for r in records if r["kind"] == "audited" and r["pass"]}
    assert all(p["snap"] in passed and p["idea"] != three for p in portfolio), portfolio
    assert [p["search"] for p in portfolio] == sorted((p["search"] for p in portfolio), reverse=True), portfolio
    assert all(p["search"] > -49 and p["select"] >= -25 for p in portfolio), portfolio  # nothing the held-out veto rejects
    x = int((path / "x.txt").read_text())
    assert report["select"] == portfolio[0]["select"] == -(x - 5) ** 2, (x, report["select"], portfolio)
    assert all((run / "portfolio" / f"c{p['idea']}" / "x.txt").exists() for p in portfolio)
    check("the result is a portfolio of distinct audited candidates the held-out score did not veto, best search first")

    assert [r["n"] for r in rows] == list(range(1, len(rows) + 1)), [r["n"] for r in rows]
    for r in rows:
        snap = json.loads((run / "snaps" / f"{r['snap']}.json").read_text())
        assert r["score"] == -(int(base64.b64decode(snap["x.txt"])) - 7) ** 2, r
    resets = [o for o in outputs(requests) if o.startswith("RESET ") or "RESET " in o]
    assert len(resets) >= 8 and all(o.split("RESET ")[1].split()[0] == o.split("RESET ")[1].split()[1] for o in resets), resets
    check("ledger rows stay numbered under concurrency, every score matches its snapshot, and ar.reset is per worker")


def scenario_01():
    # 1b. While refining keeps making progress, rounds go on; the score tops out at 12, and then the run stops by itself.
    # Each refining round adds 1, below min_delta = 1.5: progress only counts once two steps add up, so a run that judged
    # each round alone would stop after round 3.
    path = workspace("climb", {"x.txt": "0\n", "search.py": CLIMB, "select.py": CLIMB}, workers(5, extra="min_delta = 1.5\n"))
    with harness.provider(climbing) as (url, _):
        result = ar(path, url)
    report = report_of(result)
    assert report["rounds"] > 4 and "two rounds in a row made no progress" in result.stderr, (report["rounds"], result.stderr[-1500:])
    assert report["search"] == 12, report["search"]
    check("small refinement steps add up to progress; a run that keeps progressing goes on until progress stops")

    records = [json.loads(l) for l in (ar_runs(path) / report["run"] / "journal.jsonl").read_text().splitlines()]
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
    with harness.provider(hopeless) as (url, _):
        result = ar(path, url)
    report = report_of(result)
    run = ar_runs(path) / report["run"]
    records = [json.loads(l) for l in (run / "journal.jsonl").read_text().splitlines()]
    gates = [r for r in records if r["kind"] == "gate"]
    assert len(gates[0]["chosen"]) == 3 and not any(c["good"] for c in gates[0]["candidates"]), gates[0]
    copy = next(c for c in gates[0]["candidates"] if c["frontier"]["search"] == -49)
    first = [c["idea"] for c in sorted(gates[0]["candidates"], key=lambda c: (-c["frontier"]["search"], c["idea"]))][:3]
    assert copy["idea"] == gates[0]["chosen"][0] and not copy["good"], gates[0]  # a tie with the seed: not good, ranked by search
    assert gates[0]["chosen"] == first, (gates[0]["chosen"], first)
    round2 = next(r for r in records if r["kind"] == "planned" and r["batch"] == "round2")
    assert {p["role"] for p in round2["episodes"]} == {"exploit"} and {p["idea"] for p in round2["episodes"]} == set(first), round2
    assert report["portfolio"] == [] and (path / "x.txt").read_text() == "0\n" and "no candidate beat the seed" in result.stderr, report
    assert (path / "x.txt").stat().st_mtime_ns == before, "the workspace was rewritten although the result is the seed"
    assert not any(p.name.startswith(("a", "f")) for p in (run / "episodes").iterdir()), "a candidate below the seed was audited"
    check("ideas below the seed (and a tie with it) go on to refinement unaudited; a seed result leaves the workspace untouched")

    path = workspace("invalid")
    with harness.provider(lambda body, index: hopeless(body, index, "nope")) as (url, _):
        result = ar(path, url)
    report = report_of(result)
    assert report["rounds"] == 1 and report["portfolio"] == [] and (path / "x.txt").read_text() == "0\n", report
    check("with no valid idea after round 1 the run stops and the workspace keeps the seed")


def scenario_03():
    # 2c. An auditor that gives no verdict leaves its candidate unaudited: nothing becomes a result without a passing audit.
    path = workspace("noverdict")
    with harness.provider(lambda body, index: audit(body, None) if role_of(body)[0] == "audit" else scripted(body, index)) as (url, _):
        report = report_of(ar(path, url))
    records = [json.loads(l) for l in (ar_runs(path) / report["run"] / "journal.jsonl").read_text().splitlines()]
    assert any(r["kind"] == "planned" and r["batch"].startswith("audit") for r in records), "no audit ran"
    assert not any(r["kind"] == "audited" for r in records) and report["portfolio"] == [], report["portfolio"]
    assert (path / "x.txt").read_text() == "0\n" and "no verdict; it stays unaudited" in (BASE / "noverdict.stderr").read_text()
    check("an auditor that gives no verdict leaves its candidate unaudited, never a result")


def scenario_04():
    # 3. A select evaluator that cannot score the seed stops the run before any episode.
    path = workspace("badselect", spec='editable = ["x.txt"]\neval = "python3 search.py"\nselect = "exit 3"\ntimeout = 30\n')
    with harness.provider(scripted) as (url, requests):
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
        with harness.provider(once(tamper)) as (url, _):
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
    with harness.provider(once(snoop)) as (url, requests):
        ar(path, url)
    leak = [o for o in outputs(requests) if "leak" in o]
    assert leak and "[None, None]" in leak[0], leak
    check("evaluators run with a scrubbed environment")


def scenario_07():
    # File tools cannot read owner state in either policy mode, and shell commands cannot see it (nothing of the run
    # store is in the episode's copy; the run directory is hidden by Landlock). Each episode journals into the run.
    for mode in ("default", "autopilot"):
        path = workspace(f"sealed-{mode}")
        look = (f'local r = sh("ls -d .harness/log 2>/dev/null | wc -l; cat {ar_runs(path)}/*/launch.json 2>/dev/null | wc -c")\n'
                f'local ok, e = pcall(read, "{ar_runs(path)}")\n'
                'print(ar.eval("baseline"))\n'
                'return "OUT " .. r.stdout .. " READ " .. tostring(e)')
        with harness.provider(once(look, ("research", "r1-w1"))) as (url, requests):
            report = report_of(ar(path, url, extra=["--yes"] if mode == "autopilot" else []))
        outs = [o for o in outputs(requests) if "OUT " in o]
        assert len(outs) >= 2, outs
        assert all("sealed owner state" in o for o in outs), outs
        assert all(o.split("OUT ")[1].split("READ")[0].split() == ["0", "0"] for o in outs) or confined_off(path), outs
        logs = sorted((ar_runs(path) / report["run"] / "episodes").glob("*.log"))
        assert {p.name for p in logs} >= {"r0-research.log", "r1-w1.log"} and all(len(list(p.glob("*.jsonl"))) == 1 for p in logs), logs
    check("file tools and shell commands cannot reach autoresearch state; episodes journal into the run")


def scenario_08():
    # 5. A late writer cannot change what is scored: the evaluator reads a private copy of the snapshot.
    race = ('write("d.py", "import os, time\\nif os.fork() == 0:\\n    os.setsid()\\n    time.sleep(0.3)\\n    open(\'x.txt\',\'w\').write(\'7\\\\n\')\\n    os._exit(0)\\n")\n'
            'sh("python3 d.py >/dev/null 2>&1 </dev/null")\nreturn ar.eval("honest x=0")')
    path = workspace("race", spec='editable = ["x.txt"]\neval = "sleep 1; python3 search.py"\ntimeout = 30\n')
    with harness.provider(once(race)) as (url, _):
        report = report_of(ar(path, url))
    first = ledger(path, report)[0]
    snap = json.loads((ar_runs(path) / report["run"] / "snaps" / f"{first['snap']}.json").read_text())
    assert base64.b64decode(snap["x.txt"]) == b"0\n" and first["score"] == -49, (first, snap)
    check("snapshot and score always agree")


def scenario_09():
    # 6. Binary files and symlinks in an editable directory are carried or skipped, never fatal.
    files = {"pkg/a.txt": "0\n", "search.py": 'import json, os; print(json.dumps({"score": int(os.path.exists("pkg/blob.bin"))}))'}
    path = workspace("binary", files, 'editable = ["pkg"]\neval = "python3 search.py"\ntimeout = 30\n')
    junk = 'sh("printf \'\\\\377\\\\376\' > pkg/blob.bin; ln -s /etc/hosts pkg/link")\nreturn ar.eval("junk")'
    with harness.provider(once(junk)) as (url, _):
        report = report_of(ar(path, url))
    snap = json.loads((ar_runs(path) / report["run"] / "snaps" / f"{ledger(path, report)[0]['snap']}.json").read_text())
    assert base64.b64decode(snap["pkg/blob.bin"]) == b"\xff\xfe" and "pkg/link" not in snap, list(snap)
    check("binary editable files survive and symlinks are skipped")


def scenario_10():
    # 7. A killed run resumes from its journal: the interrupted worker is charged from its own journal and retried, the rest
    # of the plan continues.
    path = workspace("resume")
    with harness.provider(slow) as (url, _):
        process = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        deadline, journal = time.time() + 180, None
        while time.time() < deadline:
            journals = list(ar_runs(path).glob("*/journal.jsonl"))
            if journals:
                lines = [json.loads(l) for l in journals[0].read_text().splitlines() if l.endswith("}")]
                done = {r["label"] for r in lines if r["kind"] == "finished"}
                if sum(r["kind"] == "finished" and r["label"].startswith("r1-") for r in lines) >= 7 and "r1-w6" not in done:
                    journal = journals[0]
                    break
            time.sleep(0.05)
        assert journal, "never caught round 1 with a worker in flight"
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        for torn in (journal, journal.parent / "ledger.jsonl"):
            with open(torn, "a") as f:
                f.write('{"kind": "fini')
        (path / ".harness/ar.toml").write_text((path / ".harness/ar.toml").read_text() + "# edited after launch\n")
        # The edited ar.toml is not trusted; resume runs on launch.json, frozen in the run directory out of every agent's reach.
        untrusted = {k: v for k, v in harness.ENV.items() if k != "HX_TRUST"}
        result = subprocess.run([str(harness.HX), "run", "--ar", "--resume", str(journal.parent), "--workspace", str(path)],
                                env=untrusted, capture_output=True, text=True, timeout=600)
        assert result.returncode == 0, result.stderr[-3000:]
    report = report_of(result)
    records = [json.loads(l) for l in journal.read_text().splitlines()]
    interrupted = [r for r in records if r["kind"] == "finished" and r["outcome"] == "interrupted"]
    assert [r["label"] for r in interrupted] == ["r1-w6"], interrupted
    killed = next(r for r in records if r["kind"] == "started" and r["label"] == "r1-w6")
    assert not Path(killed["scratch"]).exists(), "killed episode's scratch copy left behind"
    assert any(r["kind"] == "started" and r["label"] == "r1-w6.2" for r in records), "the interrupted worker was not retried"
    run = journal.parent
    mine = [json.loads(l) for l in (run / "claims.jsonl").read_text().splitlines() if json.loads(l)["worker"].startswith("r1-w6")]
    assert [c["worker"] for c in mine] == ["r1-w6"], mine
    assert any(r["episode"] == "r1-w6.2" and r["idea"] == mine[0]["n"] for r in ledger(path, report)), "the retry did not keep its claim"
    assert sum(r["kind"] == "planned" and r["batch"] == "round1" for r in records) == 1 and report["rounds"] >= 3
    assert "resumed" in result.stderr
    check("a killed run resumes past torn lines with its frozen spec, charging and retrying the interrupted worker")


def scenario_11():
    # 7b. A run killed during an audit re-runs that audit on resume; no candidate passes unaudited.
    path = workspace("resume-audit")
    with harness.provider(slow_audit) as (url, _):
        process = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        deadline, journal = time.time() + 180, None
        while time.time() < deadline:
            journals = list(ar_runs(path).glob("*/journal.jsonl"))
            if journals and AUDIT_GATE.exists():
                lines = [json.loads(l) for l in journals[0].read_text().splitlines() if l.endswith("}")]
                if any(r["kind"] == "started" and r["label"].startswith("a1-") for r in lines):
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
    records = [json.loads(l) for l in journal.read_text().splitlines()]
    audited = [r for r in records if r["kind"] == "audited"]
    assert audited and not any("no verdict" in r["reason"] for r in audited), audited
    assert sum(r["kind"] == "planned" and r["batch"] == "audit1" for r in records) == 1
    rerun = [r["label"] for r in records if r["kind"] == "started" and r["label"].startswith("a1-") and "." in r["label"]]
    assert rerun, [r["label"] for r in records if r["kind"] == "started"]
    check("a run killed during an audit re-runs it on resume; no candidate passes unaudited")


def scenario_12():
    # 7c. Stopping the orchestrator stops everything it started: episode children and the commands they run.
    path = workspace("stop")
    marker = f"sleep 47{os.getpid() % 100}"
    with harness.provider(once(f'sh("{marker}")\nreturn "late"')) as (url, _):
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
    with harness.provider(lambda body, index: "Answer: 0" if role_of(body)[0] == "judge" else "nothing to do") as (url, _):
        result = ar(path, url, ok=False)
    episodes = list(ar_runs(path).glob("*/episodes/*.md"))
    assert result.returncode != 0 and "made no evaluations" in result.stderr and len(episodes) == 9, (len(episodes), result.stderr[-800:])
    check("idle agents end the run after round 1; a run without evals fails")


def scenario_14():
    # 9. Provider crashes are charged from the child's journal and retried; misconfiguration fails fast and nonzero.
    path = workspace("crash")
    with harness.provider(limited) as (url, _):
        result = ar(path, url, env={"HX_AR_RETRY_SECS": "0"})
    report = report_of(result)
    assert "retrying" in result.stderr and report["evals"] > 0, result.stderr[-1500:]
    records = [json.loads(l) for l in (ar_runs(path) / report["run"] / "journal.jsonl").read_text().splitlines()]
    crashed = next(r for r in records if r["kind"] == "finished" and r["outcome"] == "crashed")
    assert any(r["kind"] == "started" and r["label"] == "r0-research.2" for r in records), records
    path = workspace("misconfigured")
    start = time.time()
    with harness.provider(scripted) as (url, _):
        result = ar(path, url, ok=False, env={"HX_AR_RETRY_SECS": "0"}, key="HX_AR_NO_SUCH_KEY")
    assert result.returncode != 0 and "keep failing" in result.stderr and time.time() - start < 120, result.stderr[-1500:]
    check("crashes are charged from child journals and retried; misconfiguration fails fast")


def scenario_16():
    # 12. Search and select invoke their configured evaluator commands.
    grader, holdout = held_out_roots()
    spec = (f'editable = ["x.txt"]\neval = "python3 search.py"\nselect = "sh {grader}/score.sh {holdout}"\n'
            f'grader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\n')
    path = workspace("holdout", spec=spec)
    with harness.provider(once('return ar.eval("baseline")')) as (url, _):
        report = report_of(ar(path, url))
    assert all(r["score"] == -49 for r in ledger(path, report)) and report["select"] == 1, (ledger(path, report), report["select"])
    # A child's frozen policy (in its journal, in the run directory) never names the held-out evaluator.
    episodes = ar_runs(path) / report["run"] / "episodes"
    seen = [p for p in episodes.rglob("*") if p.is_file() and not p.name.endswith((".report.json", ".md", ".stderr"))]
    assert seen and not any(b"score.sh" in p.read_bytes() for p in seen), [p.name for p in seen if b"score.sh" in p.read_bytes()]
    check("search and select invoke their configured evaluator commands")


def scenario_17():
    # --yes episodes scrub credentials and file tools still seal owner and grader data.
    grader, holdout = held_out_roots()
    spec = f'editable = ["x.txt"]\neval = "python3 search.py"\ngrader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\n'
    path = workspace("autopilot-seal", spec=spec)
    (ar_runs(path) / "old-run").mkdir(parents=True)
    (ar_runs(path) / "old-run/ledger.jsonl").write_text("secret ledger")
    look = ('local r = sh("env | grep -c -E \'HX_TEST_KEY|HX_AR_EPISODE\'")\n'
            f'local a, ae = pcall(read, "{ar_runs(path)}/old-run/ledger.jsonl")\n'
            f'local b, be = pcall(read, "{holdout}/secret.txt")\n'
            f'local c, ce = pcall(write, "{grader}/score.sh", "x")\n'
            'print(ar.eval("b"))\nreturn "OUT " .. r.stdout .. tostring(ae) .. tostring(be) .. tostring(ce)')
    with harness.provider(once(look)) as (url, requests):
        ar(path, url, extra=["--yes"])
    outs = [o for o in outputs(requests) if "OUT " in o]
    assert outs and all(o.split("OUT ")[1].lstrip().startswith("0") and o.count("sealed owner state") == 3 for o in outs), outs
    assert (grader / "score.sh").read_text().count("\n") == 1, "grader root was modified"
    check("--yes episodes scrub credentials and file tools seal owner and grader data")


def scenario_18():
    # 14. Detached processes of agents and evaluators are reaped when their scratch copy is deleted.
    spec = 'editable = ["x.txt", "pid.txt"]\neval = "python3 search.py; (setsid sleep 300 >/dev/null 2>&1 & echo $! > /dev/null)"\ntimeout = 30\n'
    files = {"x.txt": "0\n", "pid.txt": "", "search.py": 'import json, subprocess, os\np = subprocess.Popen(["python3", "-c", "import os, time; os.setsid(); time.sleep(300)"], start_new_session=True)\nprint(json.dumps({"score": 0, "report": {"evaluator_daemon": p.pid}}))'}
    path = workspace("reap", files, spec)
    daemon = ('write("d.py", "import os, time\\nif os.fork() == 0:\\n    os.setsid()\\n    open(\'pid.txt\', \'w\').write(str(os.getpid()))\\n    time.sleep(300)\\nos._exit(0)\\n")\n'
              'sh("python3 d.py >/dev/null 2>&1 </dev/null; sleep 0.5")\nreturn ar.eval("daemon")')
    with harness.provider(once(daemon)) as (url, requests):
        report = report_of(ar(path, url))
    snap = ledger(path, report)[0]["snap"]
    pids = [int(base64.b64decode(json.loads((ar_runs(path) / report["run"] / "snaps" / f"{snap}.json").read_text())["pid.txt"]))]
    pids += [int(m) for o in outputs(requests) for m in re.findall(r"evaluator_daemon=(\d+)", o)]
    assert len(pids) >= 2, (pids, outputs(requests))
    time.sleep(0.5)
    # A killed orphan stays a zombie until init reaps it, which some container inits do lazily: zombies are dead.
    def state(p):
        try:
            return Path(f"/proc/{p}/stat").read_text().rsplit(")", 1)[1].split()[0]
        except OSError:
            return ""
    alive = [p for p in pids if subprocess.run(["kill", "-0", str(p)], capture_output=True).returncode == 0 and state(p) != "Z"]
    assert not alive, f"detached processes survived: {alive}"
    check("detached agent and evaluator processes are reaped")


def scenario_19():
    # 15. Snapshot limits: too many files or too deep a tree is refused per eval, never fatal to the run.
    files = {"pkg/a.txt": "0\n", "search.py": 'import json; print(json.dumps({"score": 1}))'}
    path = workspace("limits", files, 'editable = ["pkg"]\neval = "python3 search.py"\ntimeout = 30\n')
    code = ('sh("mkdir -p pkg/many && cd pkg/many && seq 1 10001 | xargs touch")\nlocal ok1, e1 = pcall(ar.eval, "many")\n'
            'sh("rm -rf pkg/many; mkdir -p pkg/$(printf \'d/%.0s\' $(seq 1 40))")\nlocal ok2, e2 = pcall(ar.eval, "deep")\n'
            'sh("rm -rf pkg/d")\nprint(ar.eval("fine"))\nreturn "E1 " .. tostring(e1) .. " E2 " .. tostring(e2)')
    with harness.provider(once(code)) as (url, requests):
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
    with harness.provider(once('return ar.eval("b")')) as (url, _):
        report = report_of(ar(path, url))
    assert ledger(path, report)[0]["score"] == 1, ledger(path, report)
    check("the frozen base ignores export attributes")


def scenario_21():
    # 16b. A deterministic evaluator scores each state once; ar.reset() restores the best state.
    code = ('print("A", ar.eval("baseline").cached)\nwrite("x.txt", "6\\n")\nprint("B", ar.eval("six").improved)\n'
            'write("x.txt", "1\\n")\nprint("C", ar.eval("one").improved)\nprint("R", ar.reset().best)\n'
            'print("X", read("x.txt"))\nlocal r = ar.eval("six again")\nreturn "D " .. tostring(r.cached) .. " " .. r.n')
    path = workspace("reset")
    with harness.provider(once(code, ("r1-w1",))) as (url, requests):
        report = report_of(ar(path, url))
    out = " ".join([o for o in outputs(requests) if "D " in o][0].split())
    assert "A nil" in out and "B true" in out and "C false" in out and "R -1" in out and "X 6" in out and "D true 2" in out, out
    rows = ledger(path, report)
    assert [r["note"] for r in rows[:3]] == ["baseline", "six", "one"] and not any(r["note"] == "six again" for r in rows), rows
    check("re-scoring a scored state returns the stored result without a ledger row, and ar.reset restores the best")


def scenario_22():
    # 16c. ar.reset never writes or deletes through a symlink the agent planted in an editable path's parent.
    outside = BASE / "outside"
    outside.mkdir()
    (outside / "x.txt").write_text("keep\n")
    code = (f'print(ar.eval("baseline"))\nsh("rm -rf pkg && ln -s {outside} pkg")\n'
            'local ok, e = pcall(ar.reset)\nreturn "RESET " .. tostring(ok) .. " " .. tostring(e)')
    path = workspace("reset-link", {"pkg/x.txt": "0\n", "search.py": 'import json; print(json.dumps({"score": 1}))'},
                     'editable = ["pkg/x.txt"]\neval = "python3 search.py"\ntimeout = 30\n')
    with harness.provider(once(code, ("r1-w1",))) as (url, requests):
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


def scenario_24():
    # 7d. Resume takes only this workspace's runs in the state directory (a run kept in the workspace, where shell
    # commands could rewrite it, is refused), a live run cannot be resumed twice, and a scratch path planted in the
    # journal is never killed in or deleted.
    path = workspace("tamper")
    with harness.provider(lambda body, index: hopeless(body, index, "nope")) as (url, _):
        report = report_of(ar(path, url))
        run = ar_runs(path) / report["run"]
        (run / "report.json").unlink()
        resume = [str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)]
        old = path / ".harness/ar" / report["run"]
        shutil.copytree(run, old)
        tampered = json.loads((old / "launch.json").read_bytes())
        tampered["spec"]["eval"] = "touch TAMPERED; python3 search.py"
        (old / "launch.json").write_text(json.dumps(tampered))
        result = subprocess.run(resume[:4] + [str(old)] + resume[5:], env=harness.ENV, capture_output=True, text=True, timeout=600)
        assert result.returncode != 0 and "predates that and cannot be resumed" in result.stderr, result.stderr[-1500:]
        listed = subprocess.run([str(harness.HX), "runs", "--workspace", str(path)], env=harness.ENV, capture_output=True, text=True)
        assert f"{run}  autoresearch, unfinished" in listed.stdout, listed.stdout

        holder = subprocess.Popen(["python3", "-c", "import fcntl, sys, time; f = open(sys.argv[1], 'w'); "
                                   "fcntl.flock(f, fcntl.LOCK_EX); print('held', flush=True); time.sleep(60)", str(run / "run.lock")],
                                  stdout=subprocess.PIPE, text=True)
        try:
            assert holder.stdout.readline().strip() == "held"
            result = subprocess.run(resume, env=harness.ENV, capture_output=True, text=True, timeout=600)
            assert result.returncode != 0 and "in use by another hx" in result.stderr, result.stderr[-1500:]
        finally:
            holder.kill()
            holder.wait()

        victim = BASE / "tamper-victim"
        victim.mkdir()
        (victim / "keep.txt").write_text("keep")
        with open(run / "journal.jsonl", "a") as f:
            f.write(json.dumps({"kind": "started", "label": "r1-w9", "plan": "r1-w9", "scratch": str(victim)}) + "\n")
        # By run id, as `hx runs` shows it.
        result = subprocess.run(resume[:4] + [report["run"]] + resume[5:], env=harness.ENV, capture_output=True, text=True, timeout=600)
        assert result.returncode == 0, result.stderr[-1500:]
        assert (victim / "keep.txt").read_text() == "keep" and "ignoring invalid recorded scratch" in result.stderr, result.stderr[-1500:]
        assert not (path / "TAMPERED").exists()
    check("resume refuses a run kept in the workspace and a run in use, and never deletes a scratch path planted in the journal")


def scenario_fallback():
    # A refinement whose state fails its audit costs only itself: the candidate falls back to its best state that passed.
    def strict(body, index):
        if role_of(body)[0] == "audit" and '"x": 7' in text_of(body).split("<starting_state>")[1].split("</starting_state>")[0]:
            return audit(body, False, "x is seven")
        return scripted(body, index)

    path = workspace("fallback")
    with harness.provider(strict) as (url, _):
        report = report_of(ar(path, url))
    run = ar_runs(path) / report["run"]
    records = [json.loads(l) for l in (run / "journal.jsonl").read_text().splitlines()]
    claims = [json.loads(l) for l in (run / "claims.jsonl").read_text().splitlines()]
    six = next(c["n"] for c in claims if c["scope"] == "explore" and c["idea"] == go(6))
    assert any(r["kind"] == "audited" and r["idea"] == six and not r["pass"] for r in records), "the x = 7 state was never failed"
    kept = next(p for p in report["portfolio"] if p["idea"] == six)
    snap = json.loads((run / "snaps" / f"{kept['snap']}.json").read_text())
    assert base64.b64decode(snap["x.txt"]) == b"6\n" and any(r["kind"] == "audited" and r["snap"] == kept["snap"] and r["pass"] for r in records), kept
    check("a refinement that fails its audit costs only itself: the candidate keeps its best state that passed")


# Review findings: the gate, interrupts, confinement, the watchdog, --finish and the binary episodes run. These use the
# linear task (search = x) and few workers, so each takes seconds.
LINEAR = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": %s, "report": {"x": x}}))'
CONST = 'import json; print(json.dumps({"score": 0}))'


def lanes(steps, verdicts=lambda x: True, other=None):
    """A provider whose workers follow `steps` ({plan label: [x, ...]}: claim, then score each x in turn) and whose
    auditors judge the x of the state they audit (`verdicts(x)`: True, False, or None for no verdict). Episodes without
    a lane go to `other` if given, else finish at once."""
    def reply(body, index):
        role, label = role_of(body)
        if role == "judge": return "Answer: 0"
        if role == "audit":
            x = int(re.search(r'"x": (-?\d+)', text_of(body).split("<starting_state>")[1]).group(1))
            v = verdicts(x)
            return audit(body, v, "" if v else f"x = {x} breaks the rules")
        steps_ = steps.get((label or "").split(".")[0])
        if steps_ is None: return other(body, index) if other else ("BRIEF" if role == "research" else "done")
        if answered(body): return f"FINDING from {label}"
        code = f'ar.claim("move x through {steps_} as worker {label} does it today")\n'
        return {"code": code + "".join(f'write("x.txt", "{x}\\n")\nprint(ar.eval("x={x}"))\n' for x in steps_) + 'return "ok"'}
    return reply


def scenario_veto():
    # Finding 1: a refinement the held-out veto rejects costs only itself; the candidate keeps its passed result, its
    # next workers are told why, and max_rounds ends the run.
    path = workspace("veto", {"x.txt": "0\n", "search.py": LINEAR % "x", "select.py": SCORE % 5}, workers(2, 3))
    with harness.provider(lanes({"r1-w1": [3], "r2-w1": [11], "r2-w2": [4]})) as (url, requests):
        result = ar(path, url)
    report, records = report_of(result), journal_of(path)
    rows = ledger(path, report)
    x11 = next(r["snap"] for r in rows if r["note"] == "x=11")
    assert [r["score"] for r in records if r["kind"] == "selected" and r["snap"] == x11] == [-36], records
    gates = [r for r in records if r["kind"] == "gate"]
    assert all(c["frontier"]["snap"] != x11 for g in gates for c in g["candidates"]), gates
    assert not any(p["start"] == x11 for r in records if r["kind"] == "planned" for p in r["episodes"])
    task = (ar_runs(path) / report["run"] / "episodes/r3-w1.md").read_text()
    assert "was vetoed" in task and "-36" not in task, task.split("<checks>")[-1][:500]
    assert [snap_x(path, report, p["snap"]) for p in report["portfolio"]] == [4] and (path / "x.txt").read_text() == "4\n", report["portfolio"]
    assert "3 rounds, the most this run takes" in result.stderr and gates[-1]["last"], result.stderr[-800:]
    check("a vetoed refinement falls back like a failed audit: the candidate keeps a result and its workers learn why (finding 1)")


def scenario_noverdict_keeps_result():
    # Finding 1: an audit that ends without a verdict leaves the candidate's earlier passed result in place.
    path = workspace("noverdict2", {"x.txt": "0\n", "search.py": LINEAR % "x", "select.py": LINEAR % "x"}, workers(2, 2))
    with harness.provider(lanes({"r1-w1": [3], "r2-w1": [4]}, lambda x: True if x == 3 else None)) as (url, _):
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
    with harness.provider(lanes({"r1-w1": [3, 6, 5]}, lambda x: x != 6)) as (url, _):
        result = ar(path, url)
    report, records = report_of(result), journal_of(path)
    round2 = next(r for r in records if r["kind"] == "planned" and r["batch"] == "round2")
    assert snap_x(path, report, round2["episodes"][0]["start"]) == 5, round2
    task = (ar_runs(path) / report["run"] / "episodes/r2-w1.md").read_text()
    assert "failed its audit: x = 6 breaks the rules" in task, task[:2000]
    assert [snap_x(path, report, p["snap"]) for p in report["portfolio"]] == [5] and (path / "x.txt").read_text() == "5\n", report
    check("a failed audit falls back to the best surviving state, and its reason reaches the candidate's workers (findings 7, 8)")


def scenario_tie():
    # Finding 9: a state that ties the seed's score is a candidate like any other: not good, ranked by search.
    path = workspace("tie", {"x.txt": "0\n", "search.py": LINEAR % "x // 10", "select.py": CONST}, workers(2, 2))
    with harness.provider(lanes({"r1-w1": [5], "r1-w2": [-5]})) as (url, _):
        result = ar(path, url)
    report, records = report_of(result), journal_of(path)
    gate = next(r for r in records if r["kind"] == "gate")
    ties = [c for c in gate["candidates"] if c["frontier"]["search"] == 0]
    assert len(ties) == 1 and not ties[0]["good"] and gate["chosen"][0] == ties[0]["idea"] and len(gate["chosen"]) == 2, gate
    round2 = next(r for r in records if r["kind"] == "planned" and r["batch"] == "round2")
    assert {p["idea"] for p in round2["episodes"]} == set(gate["chosen"]), round2
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
        return "Answer: 0" if role == "judge" else "BRIEF" if role == "research" else "done"
    def started(n):
        deadline = time.time() + 120
        while time.time() < deadline:
            js = list(ar_runs(path).glob("*/journal.jsonl"))
            if js and sum(r["kind"] == "started" and r["label"].startswith("r1-") for r in
                          (json.loads(l) for l in js[0].read_text().splitlines() if l.endswith("}"))) >= n:
                return js[0].parent
            time.sleep(0.1)
        raise AssertionError(f"round 1 never started {n} attempts")
    resume = lambda run: [str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)]
    with harness.provider(sleepy) as (url, _):
        p = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        for n in (2, 4, 6):
            run = started(n)
            time.sleep(1)
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
            if n < 6:
                p = subprocess.Popen(resume(run), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        fixed.append(True)
        result = subprocess.run(resume(run), env=harness.ENV, capture_output=True, text=True, timeout=300)
    assert result.returncode == 0, result.stderr[-2000:]
    records = journal_of(path)
    assert [r["label"] for r in records if r["kind"] == "started" and r["label"].startswith("r1-w1")] == ["r1-w1", "r1-w1.2", "r1-w1.3", "r1-w1.4"]
    killed = [r for r in records if r["kind"] == "finished" and r["outcome"] == "interrupted"]
    assert len(killed) == 6 and [g["chosen"] for g in records if g["kind"] == "gate"][0], records
    for r in killed:
        scratch = next(s["scratch"] for s in records if s["kind"] == "started" and s["label"] == r["label"])
        session = (run / "episodes" / f"{r['label']}.log").read_text() if (run / "episodes" / f"{r['label']}.log").is_file() else \
            "".join(j.read_text() for j in (run / "episodes" / f"{r['label']}.log").glob("*.jsonl"))
        tmp = re.search(r'"tmp":"([^"]+)"', session).group(1)
        assert tmp.startswith(scratch + "/tmp/") and not Path(tmp).exists() and not Path(scratch).exists(), (tmp, scratch)
    check("interrupted attempts are not crashes: a run stopped three times mid-round still finishes, and killed children leave no temp directory (findings 2, 15)")


def scenario_unconfined():
    # Finding 3: without Landlock a spec with held-out or grader data refuses to launch or resume, unless acknowledged.
    grader, holdout = held_out_roots()
    spec = (f'editable = ["x.txt"]\neval = "python3 search.py"\nselect = "sh {grader}/score.sh {holdout}"\n'
            f'grader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\nworkers = 1\nmax_rounds = 1\n')
    path = workspace("unconfined", spec=spec)
    off = {"HX_TEST_NO_LANDLOCK": "1"}
    with harness.provider(once('return ar.eval("b")', ("r1-w1",))) as (url, requests):
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
    with harness.provider(stuck) as (url, _):
        result = ar(path, url, env={"HX_AR_WATCHDOG_SECS": "4"})
    records = journal_of(path)
    research = [r for r in records if r["kind"] == "finished" and r["label"].startswith("r0-research")]
    assert [(r["label"], r["outcome"]) for r in research] == [("r0-research", "timed_out")], research
    assert "outlived its" in result.stderr and report_of(result)["evals"] >= 1, result.stderr[-800:]
    check("the watchdog kills a child past its cap and journals it as timed out, not crashed (finding 11)")


def scenario_finish():
    # Finding 14: `--resume RUN --finish` writes the result from the last gate and starts nothing.
    path = workspace("finish", {"x.txt": "0\n", "search.py": LINEAR % "x", "select.py": LINEAR % "x"}, workers(2))
    slow = lambda body, index: {"code": 'sh("sleep 60")\nreturn "late"'} if role_of(body)[0] == "exploit" and not answered(body) else "done"
    with harness.provider(lanes({"r1-w1": [3]}, other=slow)) as (url, requests):
        p = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        deadline = time.time() + 120
        while time.time() < deadline and not any(r["kind"] == "started" and r["label"] == "r2-w1" for r in (journal_of(path) if list(ar_runs(path).glob("*/journal.jsonl")) else [])):
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
    with harness.provider(scripted) as (url, requests):
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
    with harness.provider(reply) as (url, requests):
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
        if any("PWD " in o or "RECORDED" in o for o in outs): return audit(body, True)
        if outs and "[hx] The run stops" in outs[-1]: return verdict_code(True)
        return {"code": 'return "still looking"'}
    with harness.provider(reply) as (url, _):
        report = report_of(ar(path, url))
    records = journal_of(path)
    assert max(turns) >= 30 and [r["pass"] for r in records if r["kind"] == "audited"] == [True], (max(turns), records[-3:])
    assert [snap_x(path, report, p["snap"]) for p in report["portfolio"]] == [3], report
    check("an auditor that records its verdict right after the turn-cap notice is audited")


# Scenarios are independent (own workspace, own mock provider), so a few run side by side; more would load the machine.
with ThreadPoolExecutor(max_workers=4) as pool:
    for future in [pool.submit(f) for f in [v for k, v in sorted(globals().items()) if k.startswith("scenario_")]]:
        future.result()
print(f"{passed}/{passed} ar checks passed")
