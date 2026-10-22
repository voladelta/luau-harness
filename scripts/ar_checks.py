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
import signal
import subprocess
import time
import redesign_checks as harness
from pathlib import Path

BASE = harness.BASE / "ar"
BASE.mkdir(exist_ok=True)
PRICE = {"prompt": "0.00001", "completion": "0.00001"}
SCORE = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": -(x - %d) ** 2, "report": {"x": x}}))'
passed = 0


def check(name):
    global passed
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


def command(path, url, budget, key="HX_TEST_KEY"):
    return [str(harness.HX), "run", "--ar", "--workspace", str(path), "--prompt-file", str(path / "program.md"),
            "--model", "mock", "--base-url", url, "--key-env", key, "--max-cost", str(budget)]


def ar(path, url, budget, ok=True, env=None, key="HX_TEST_KEY", extra=()):
    result = subprocess.run(command(path, url, budget, key) + list(extra), env={**harness.ENV, **(env or {})},
                            capture_output=True, text=True, timeout=600)
    (BASE / f"{path.name}.stderr").write_text(result.stderr)
    if ok:
        assert result.returncode == 0, (result.returncode, result.stdout[-2000:], result.stderr[-4000:])
    return result


def report_of(result):
    return json.loads(result.stdout.strip().splitlines()[-1])


def ledger(path, report):
    return [json.loads(line) for line in (path / ".harness/ar" / report["run"] / "ledger.jsonl").read_text().splitlines()]


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
    if answered(body):
        return f"FINDING from {label}: done"
    if role == "research":
        return "BRIEF: the score rewards x close to the hidden target; avoid huge x."
    if role == "audit":
        return "Checked the state.\nVERDICT: FAIL: probe trick" if f"({go(3)})" in text_of(body) else "Checked.\nVERDICT: PASS"
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


path = workspace("rounds")
with harness.provider(scripted, price=PRICE) as (url, requests):
    result = ar(path, url, 0.6)
report = report_of(result)
run = path / ".harness/ar" / report["run"]
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
gates = [r for r in records if r["kind"] == "gate"]
assert [g["round"] for g in gates] == [1, 2, 3, 4], gates
three = next(c["n"] for c in explore if c["idea"] == go(3))
six = next(c["n"] for c in explore if c["idea"] == go(6))
assert gates[0]["chosen"][0] == six and three not in gates[0]["chosen"] and len(gates[0]["chosen"]) == 3, gates[0]
assert any(r["kind"] == "audited" and r["idea"] == three and not r["pass"] for r in records)
assert not any(c["idea"] == three for g in gates for c in g["candidates"] if c["idea"] in g["chosen"])
bad = {c["n"] for c in explore if c["idea"] in (go(20), go(40 + loser), go(12), go(13))}
vetoed = [c for c in gates[0]["candidates"] if c["idea"] in bad and c["search"] > -49]
assert vetoed and all(c["select"] < -25 for c in vetoed), vetoed  # beat the seed on search, lose on held-out
assert all(not c["good"] for c in gates[0]["candidates"] if c["idea"] in bad), gates[0]["candidates"]
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
# Round 4's new ideas don't move the chosen candidates either, so the run stops with budget left.
roles = {r["batch"]: r["episodes"][0]["role"] for r in records if r["kind"] == "planned" and r["batch"].startswith("round")}
assert roles == {"round1": "explore", "round2": "exploit", "round3": "exploit", "round4": "explore"}, roles
everything = [c["idea"] for c in claims if c["scope"] == "explore"]
assert len(everything) > 8 and len(set(everything)) == len(everything), everything
r4 = (run / "episodes/r4-w1.md").read_text().split("Earlier rounds already claimed these ideas")[1].split("\n2. ")[0]
assert all(f"#{c['n']}:" in r4 for c in explore) and not any(f"#{c['n']}:" in r4 for c in claims if c["worker"].startswith("r4-")), r4
assert "Earlier rounds" not in (run / "episodes/r1-w1.md").read_text()
assert "two rounds in a row made no progress" in result.stderr and report["max_cost_usd"] == 0.6, report
check("a refining round that moves no chosen candidate is followed by an explore round; two rounds without progress stop the run")

# Replaying the finished run from its journal reaches the same stop without starting anything.
(run / "report.json").unlink()
before = (run / "journal.jsonl").read_text()
with harness.provider(scripted, price=PRICE) as (url, replayed):
    replay = subprocess.run([str(harness.HX), "run", "--ar", "--resume", str(run), "--workspace", str(path)],
                            env=harness.ENV, capture_output=True, text=True, timeout=600)
assert replay.returncode == 0 and not replayed, replay.stderr[-2000:]
assert (run / "journal.jsonl").read_text() == before, "the replay journaled something"
again = report_of(replay)
assert (again["rounds"], again["snap"], again["portfolio"]) == (report["rounds"], report["snap"], report["portfolio"]), again
check("a resumed run replays its rounds from the journal: same decisions, nothing re-planned or re-run")

portfolio = report["portfolio"]
assert 1 <= len(portfolio) <= 3 and len({p["snap"] for p in portfolio}) == len(portfolio), portfolio
assert all(p["audit"] is True and p["idea"] != three for p in portfolio), portfolio
assert [p["search"] for p in portfolio] == sorted((p["search"] for p in portfolio), reverse=True), portfolio
assert all(p["good"] and p["select"] >= -25 for p in portfolio), portfolio  # nothing the held-out veto rejects
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

# 1b. While refining keeps making progress, rounds go on; here only the optional cost ceiling stops them.
CLIMB = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": x}))'


limited_once = [True]


def climbing(body, index):
    role, label = role_of(body)
    if role == "judge": return judge_same(body)
    if label == "r1-w1" and limited_once[0]:
        limited_once[0] = False
        return {"status": 429, "error": "slow down"}
    if answered(body): return f"FINDING from {label}: done"
    if role == "audit": return "Checked.\nVERDICT: PASS"
    if role == "explore":
        w = int(label.split("-w")[1].split(".")[0])
        return {"code": f'ar.claim("{go(w)}")\nwrite("x.txt", "{w}\\n")\nreturn ar.eval("go {w}")'}
    if role == "exploit":
        return {"code": f'ar.claim("raise x by one more step, the refinement tried by worker {label}")\n'
                        'write("x.txt", tostring(tonumber(read("x.txt")) + 1) .. "\\n")\nreturn ar.eval("up")'}
    return "done"


# Each refining round adds 1, below min_delta = 1.5: progress only counts once two steps add up, so a run that judged
# each round alone would stop after round 3.
path = workspace("climb", {"x.txt": "0\n", "search.py": CLIMB, "select.py": CLIMB},
                 'editable = ["x.txt"]\neval = "python3 search.py"\nselect = "python3 select.py"\ntimeout = 30\nmin_delta = 1.5\n')
with harness.provider(climbing, price=PRICE) as (url, _):
    result = ar(path, url, 0.3)
report = report_of(result)
assert report["rounds"] > 4 and "made no progress" not in result.stderr, (report["rounds"], result.stderr[-1500:])
assert 0.3 <= report["cost_usd"] <= 0.3 + 0.1 and report["max_cost_usd"] == 0.3, report["cost_usd"]
assert report["search"] > 8 + 2, report["search"]
check("small refinement steps add up to progress; only the optional cost ceiling stops a run that keeps progressing")

records = [json.loads(l) for l in (path / ".harness/ar" / report["run"] / "journal.jsonl").read_text().splitlines()]
widths = [len(r["episodes"]) for r in records if r["kind"] == "planned" and r["batch"].startswith("round")]
assert widths[0] == 8 and widths[1] == 6 and max(widths[1:]) == 6, widths
limited = [r for r in records if r["kind"] == "finished" and r.get("rate_limits")]
assert [r["label"] for r in limited] == ["r1-w1"] and "1 rate-limited" in result.stderr, limited
check("eight workers per round, six once the provider rate-limits an episode")

# 2. Ideas below the seed still go on to refinement, but never become results: the workspace keeps the seed. With no
# valid idea at all the run stops after round 1.
def hopeless(body, index, x=None):
    role, label = role_of(body)
    if role == "judge": return judge_same(body)
    if answered(body) or role != "explore": return "done"
    w = int(label.split("-w")[1].split(".")[0])
    value = x or ("00" if w == 1 else 30 + w)  # worker 1 rebuilds the seed in other bytes
    return {"code": f'ar.claim("{go(30 + w)}")\nwrite("x.txt", "{value}\\n")\nreturn ar.eval("far")'}


path = workspace("hopeless")
with harness.provider(hopeless, price=PRICE) as (url, _):
    result = ar(path, url, 0.3)
report = report_of(result)
run = path / ".harness/ar" / report["run"]
records = [json.loads(l) for l in (run / "journal.jsonl").read_text().splitlines()]
gates = [r for r in records if r["kind"] == "gate"]
assert len(gates[0]["chosen"]) == 3 and not any(c["good"] for c in gates[0]["candidates"]), gates[0]
copy = next(c for c in gates[0]["candidates"] if c["search"] == -49)
first = [c["idea"] for c in sorted(gates[0]["candidates"], key=lambda c: -c["search"]) if c is not copy][:3]
assert copy["idea"] not in gates[0]["chosen"], gates[0]
assert gates[0]["chosen"] == first, (gates[0]["chosen"], first)
round2 = next(r for r in records if r["kind"] == "planned" and r["batch"] == "round2")
assert {p["role"] for p in round2["episodes"]} == {"exploit"} and {p["idea"] for p in round2["episodes"]} == set(first), round2
assert report["portfolio"] == [] and (path / "x.txt").read_text() == "0\n" and "no candidate beat the seed" in result.stderr, report
assert not any(p.name.startswith("a") for p in (run / "episodes").iterdir()), "a candidate below the seed was audited"
check("ideas below the seed go on to refinement unaudited, a copy of the seed never; the workspace keeps the seed")

path = workspace("invalid")
with harness.provider(lambda body, index: hopeless(body, index, "nope"), price=PRICE) as (url, _):
    result = ar(path, url, 0.3)
report = report_of(result)
assert report["rounds"] == 1 and report["portfolio"] == [] and (path / "x.txt").read_text() == "0\n", report
check("with no valid idea after round 1 the run stops and the workspace keeps the seed")

# 2b. When the cost ceiling is reached before a gate's audits can start, no verdict is invented and nothing unaudited
# becomes a result: the workspace keeps the seed.
def costly(body, index):
    """Explore workers' final replies are billed $0.05 each, so round 1 passes the $0.30 ceiling before any audit can
    start; their claims and evaluations run first."""
    answer = scripted(body, index)
    if role_of(body)[0] == "explore" and answered(body):
        answer = {"text": answer, "usage": {"cost": 0.05}}
    return answer


path = workspace("noaudit")
with harness.provider(costly, price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.3))
records = [json.loads(l) for l in (path / ".harness/ar" / report["run"] / "journal.jsonl").read_text().splitlines()]
finished = {r["label"] for r in records if r["kind"] == "finished" and not r.get("crashed")}
ran = {(p["idea"], p["start"]) for r in records if r["kind"] == "planned" and r["batch"].startswith("audit")
       for p in r["episodes"] if any(s["label"] in finished for s in records if s["kind"] == "started" and s["plan"] == p["label"])}
assert all((r["idea"], r["snap"]) in ran for r in records if r["kind"] == "audited"), "a verdict without a finished audit"
good = [c for c in report["candidates"] if c["good"]]
assert good and all(c["audit"] is None for c in good), report["candidates"]
assert report["portfolio"] == [] and (path / "x.txt").read_text() == "0\n", report["portfolio"]
check("good candidates the cost ceiling left no room to audit never become results, and no verdict is invented")

# 2c. An auditor that gives no verdict leaves its candidate unaudited: nothing becomes a result without a passing audit.
path = workspace("noverdict")
with harness.provider(lambda body, index: "Checked the state." if role_of(body)[0] == "audit" else scripted(body, index), price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.3))
records = [json.loads(l) for l in (path / ".harness/ar" / report["run"] / "journal.jsonl").read_text().splitlines()]
assert any(r["kind"] == "planned" and r["batch"].startswith("audit") for r in records), "no audit ran"
assert not any(r["kind"] == "audited" for r in records) and report["portfolio"] == [], report["portfolio"]
assert (path / "x.txt").read_text() == "0\n" and "no verdict; it stays unaudited" in (BASE / "noverdict.stderr").read_text()
check("an auditor that gives no verdict leaves its candidate unaudited, never a result")

# 3. A select evaluator that cannot score the seed stops the run before any episode.
path = workspace("badselect", spec='editable = ["x.txt"]\neval = "python3 search.py"\nselect = "exit 3"\ntimeout = 30\n')
with harness.provider(scripted, price=PRICE) as (url, requests):
    result = ar(path, url, 0.3, ok=False)
assert result.returncode != 0 and "select evaluator gives no valid score" in result.stderr and not requests, result.stderr[-800:]
check("a broken select evaluator fails fast, before spending")

# 2. Tampering outside the editable paths never reaches the evaluator: no Git hooks, no ignore rules, no evaluator edits.
tamper = (f'sh("git init -q; git config core.fsmonitor $PWD/hook.sh; printf \'#!/bin/sh\\\\ntouch {BASE}/pwned\\\\n\' > hook.sh; chmod +x hook.sh; echo json.py >> .git/info/exclude")\n'
          'write("search.py", "print(\'{\\"score\\": 1000}\')")\n'
          'write("json.py", "def dumps(o): return \'{\\"score\\": 999}\'\\n")\n'
          'return ar.eval("tamper")')
for mode in ("sandbox", "yolo"):
    path = workspace(f"tamper-{mode}")
    with harness.provider(once(tamper), price=PRICE) as (url, _):
        report = report_of(ar(path, url, 0.1, extra=["--yolo"] if mode == "yolo" else []))
    rows = ledger(path, report)
    assert rows and all(r["score"] == -49 for r in rows), rows
    assert (path / "search.py").read_text() == SCORE % 7
    log = subprocess.run(["git", "log", "--oneline"], cwd=path, capture_output=True, text=True).stdout
    assert len(log.splitlines()) == 1, log
assert not (BASE / "pwned").exists(), "a hook ran"
check("tampering outside the editable paths cannot change scores or the owner's repository (sandbox and yolo)")

# 3. Editable code running inside the evaluator gets no credentials and cannot read the run or the workspace.
files = {"cand.py": "value = 0\n",
         "search.py": 'import json, cand; print(json.dumps({"score": 0, "report": {"leak": repr(cand.value)}}))'}
path = workspace("envleak", files, 'editable = ["cand.py"]\neval = "python3 search.py"\ntimeout = 30\n')
snoop = (f'write("cand.py", "import os, glob\\nvalue = [os.environ.get(\'HX_TEST_KEY\'), os.environ.get(\'HX_AR_EPISODE\'), '
         f'glob.glob(\'{path}/.harness/ar/*/*\'), glob.glob(\'{path}/*\')]\\n")\n'
         'return ar.eval("snoop")')
with harness.provider(once(snoop), price=PRICE) as (url, requests):
    ar(path, url, 0.1)
leak = [o for o in outputs(requests) if "leak" in o]
assert leak and "[None, None, [], []]" in leak[0], leak
check("evaluators run with a scrubbed environment and cannot read the run or workspace")

# 4. Agents cannot read the run directory, earlier episodes' logs, or the owner's workspace, even with --yolo.
for mode in ("sandbox", "yolo"):
    path = workspace(f"sealed-{mode}")
    look = (f'local r = sh("ls -R {path}/.harness/ar 2>&1; ls .harness/log/*.jsonl | wc -l")\n'
            f'local ok, e = pcall(read, "{path}/.harness/ar")\n'
            'print(ar.eval("baseline"))\n'
            'return "OUT " .. r.stdout .. " READ " .. tostring(e)')
    with harness.provider(once(look, ("research", "r1-w1")), price=PRICE) as (url, requests):
        ar(path, url, 0.3, extra=["--yolo"] if mode == "yolo" else [])  # room for r1-w1's claim judgements and its reply
    outs = [o for o in outputs(requests) if "OUT " in o]
    assert len(outs) >= 2, outs
    assert all("snaps" not in o and "journal" not in o and "ledger" not in o for o in outs), outs
    assert all(o.split("OUT ")[1].split("READ")[0].strip().endswith("1") for o in outs), outs  # only its own journal
check("agents cannot read autoresearch state or earlier episodes (sandbox and yolo)")

# 5. A late writer cannot change what is scored: the evaluator reads a private copy of the snapshot.
race = ('write("d.py", "import os, time\\nif os.fork() == 0:\\n    os.setsid()\\n    time.sleep(0.3)\\n    open(\'x.txt\',\'w\').write(\'7\\\\n\')\\n    os._exit(0)\\n")\n'
        'sh("python3 d.py >/dev/null 2>&1 </dev/null")\nreturn ar.eval("honest x=0")')
path = workspace("race", spec='editable = ["x.txt"]\neval = "sleep 1; python3 search.py"\ntimeout = 30\n')
with harness.provider(once(race), price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.1))
first = ledger(path, report)[0]
snap = json.loads((path / ".harness/ar" / report["run"] / "snaps" / f"{first['snap']}.json").read_text())
assert base64.b64decode(snap["x.txt"]) == b"0\n" and first["score"] == -49, (first, snap)
check("snapshot and score always agree")

# 6. Binary files and symlinks in an editable directory are carried or skipped, never fatal.
files = {"pkg/a.txt": "0\n", "search.py": 'import json, os; print(json.dumps({"score": int(os.path.exists("pkg/blob.bin"))}))'}
path = workspace("binary", files, 'editable = ["pkg"]\neval = "python3 search.py"\ntimeout = 30\n')
junk = 'sh("printf \'\\\\377\\\\376\' > pkg/blob.bin; ln -s /etc/hosts pkg/link")\nreturn ar.eval("junk")'
with harness.provider(once(junk), price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.1))
snap = json.loads((path / ".harness/ar" / report["run"] / "snaps" / f"{ledger(path, report)[0]['snap']}.json").read_text())
assert base64.b64decode(snap["pkg/blob.bin"]) == b"\xff\xfe" and "pkg/link" not in snap, list(snap)
check("binary editable files survive and symlinks are skipped")

# 7. A killed run resumes from its journal: the interrupted worker is charged from its own journal and retried, the rest
# of the plan continues.
def slow(body, index):
    role, label = role_of(body)
    if role == "explore" and label == "r1-w6" and not answered(body):
        return {"code": f'print(ar.claim("{go(1)}"))\nsh("sleep 20")\nreturn "late"'}
    return scripted(body, index)


path = workspace("resume")
with harness.provider(slow, price=PRICE) as (url, _):
    process = subprocess.Popen(command(path, url, 0.6), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=True)
    deadline, journal = time.time() + 180, None
    while time.time() < deadline:
        journals = list((path / ".harness/ar").glob("*/journal.jsonl"))
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
    untrusted = {k: v for k, v in harness.ENV.items() if k != "HX_TRUST"}
    untrusted["XDG_CONFIG_HOME"] = str(BASE / "empty-trust-store")
    result = subprocess.run([str(harness.HX), "run", "--ar", "--resume", str(journal.parent), "--workspace", str(path)],
                            env=untrusted, capture_output=True, text=True, timeout=600)
    assert result.returncode == 0, result.stderr[-3000:]
report = report_of(result)
records = [json.loads(l) for l in journal.read_text().splitlines()]
interrupted = [r for r in records if r["kind"] == "finished" and r.get("interrupted")]
assert [r["label"] for r in interrupted] == ["r1-w6"] and interrupted[0]["cost"] < 0.05, interrupted
killed = next(r for r in records if r["kind"] == "started" and r["label"] == "r1-w6")
assert not Path(killed["scratch"]).exists(), "killed episode's scratch copy left behind"
assert any(r["kind"] == "started" and r["label"] == "r1-w6.2" for r in records), "the interrupted worker was not retried"
run = journal.parent
mine = [json.loads(l) for l in (run / "claims.jsonl").read_text().splitlines() if json.loads(l)["worker"].startswith("r1-w6")]
assert [c["worker"] for c in mine] == ["r1-w6"], mine
assert any(r["episode"] == "r1-w6.2" and r["idea"] == mine[0]["n"] for r in ledger(path, report)), "the retry did not keep its claim"
assert sum(r["kind"] == "planned" and r["batch"] == "round1" for r in records) == 1 and report["rounds"] >= 3
assert "resumed" in result.stderr and report["cost_usd"] <= 0.6 + 1e-9
check("a killed run resumes past torn lines with its frozen spec, charging and retrying the interrupted worker")

# 7b. A run killed during an audit re-runs that audit on resume; no candidate passes unaudited.
def slow_audit(body, index):
    role, label = role_of(body)
    if role == "audit" and label.startswith("a1-") and not answered(body) and (path / "audit-gate").exists() is False:
        (path / "audit-gate").write_text("")
        return {"code": 'sh("sleep 20")\nreturn "late"'}
    return scripted(body, index)


path = workspace("resume-audit")
with harness.provider(slow_audit, price=PRICE) as (url, _):
    process = subprocess.Popen(command(path, url, 0.6), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=True)
    deadline, journal = time.time() + 180, None
    while time.time() < deadline:
        journals = list((path / ".harness/ar").glob("*/journal.jsonl"))
        if journals and (path / "audit-gate").exists():
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

# 8. Agents that never score anything end the run after round 1, and a run with no evals fails.
path = workspace("idle")
with harness.provider(lambda body, index: "Answer: 0" if role_of(body)[0] == "judge" else "nothing to do", price=PRICE) as (url, _):
    result = ar(path, url, 0.3, ok=False)
episodes = list((path / ".harness/ar").glob("*/episodes/*.md"))
assert result.returncode != 0 and "made no evaluations" in result.stderr and len(episodes) == 9, (len(episodes), result.stderr[-800:])
check("idle agents end the run after round 1; a run without evals fails")

# 9. Provider crashes are charged from the child's journal and retried; misconfiguration fails fast and nonzero.
agent_calls = [0]


def limited(body, index):
    # Unavailable twice in a row exhausts the one retry, so the research child crashes (rate limits are waited out instead).
    if role_of(body)[0] == "research" and not answered(body):
        agent_calls[0] += 1
        if agent_calls[0] < 3: return {"status": 503, "error": "unavailable"}
    return once('return ar.eval("b")')(body, index)


path = workspace("crash")
with harness.provider(limited, price=PRICE) as (url, _):
    result = ar(path, url, 0.3, env={"HX_AR_RETRY_SECS": "0"})
report = report_of(result)
assert "retrying" in result.stderr and report["evals"] > 0, result.stderr[-1500:]
records = [json.loads(l) for l in (path / ".harness/ar" / report["run"] / "journal.jsonl").read_text().splitlines()]
crashed = next(r for r in records if r["kind"] == "finished" and r.get("crashed"))
assert crashed["cost"] < 0.01 and any(r["kind"] == "started" and r["label"] == "r0-research.2" for r in records), records
path = workspace("misconfigured")
start = time.time()
with harness.provider(scripted, price=PRICE) as (url, _):
    result = ar(path, url, 0.3, ok=False, env={"HX_AR_RETRY_SECS": "0"}, key="HX_AR_NO_SUCH_KEY")
assert result.returncode != 0 and "keep failing" in result.stderr and time.time() - start < 120, result.stderr[-1500:]
check("crashes are charged from child journals and retried; misconfiguration fails fast")

# 11. No sandboxed process can write a shared xcrun cache path (a channel between episodes); shims still run.
T = subprocess.run(["getconf", "DARWIN_USER_TEMP_DIR"], capture_output=True, text=True).stdout.strip()
probe = f"{T}xcrun_dbAR{os.getpid()}"
files = {"x.txt": "0\n", "search.py": f'import json, os\nok = True\ntry:\n    os.makedirs("{probe}e", exist_ok=True)\nexcept OSError:\n    ok = False\nprint(json.dumps({{"score": 1 if not ok else 0}}))'}
path = workspace("xcrun", files, 'editable = ["x.txt"]\neval = "python3 search.py"\ntimeout = 30\n')
code = f'local r = sh("mkdir -p {probe}a 2>&1; echo leak > {probe}a/f 2>&1; /usr/bin/git --version")\nprint(r.stdout)\nreturn ar.eval("probe")'
with harness.provider(once(code), price=PRICE) as (url, requests):
    report = report_of(ar(path, url, 0.1))
assert not os.path.exists(probe + "a") and not os.path.exists(probe + "e"), "xcrun prefix is writable"
assert any("git version" in o for o in outputs(requests)), outputs(requests)
assert ledger(path, report)[0]["score"] == 1
check("no sandbox can write the shared xcrun cache prefix; tool shims still work")

# 12. Held-out data is readable by the select evaluator only, never by a search evaluation.
grader, holdout = BASE / "g-grader", BASE / "g-holdout"
grader.mkdir()
holdout.mkdir()
(grader / "score.sh").write_text('if cat "$1/secret.txt" >/dev/null 2>&1; then echo \'{"score": 1}\'; else echo \'{"score": 0}\'; fi\n')
(holdout / "secret.txt").write_text("held-out")
spec = (f'editable = ["x.txt"]\neval = "sh {grader}/score.sh {holdout}"\nselect = "sh {grader}/score.sh {holdout}"\n'
        f'grader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\n')
path = workspace("holdout", spec=spec)
with harness.provider(once('return ar.eval("baseline")'), price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.1))
assert all(r["score"] == 0 for r in ledger(path, report)) and report["select"] == 1, (ledger(path, report), report["select"])
check("held-out roots are readable by select only")

# 13. With --yolo an episode still gets no credentials, no run location, no other runs and no grader roots.
spec = f'editable = ["x.txt"]\neval = "python3 search.py"\ngrader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\n'
path = workspace("yolo-seal", spec=spec)
(path / ".harness/ar/old-run").mkdir(parents=True)
(path / ".harness/ar/old-run/ledger.jsonl").write_text("secret ledger")
look = (f'local r = sh("env | grep -c -E \'HX_TEST_KEY|HX_AR_EPISODE\'; cat {path}/.harness/ar/old-run/ledger.jsonl 2>&1; '
        f'cat {holdout}/secret.txt 2>&1; echo x >> {grader}/score.sh 2>&1")\nprint(ar.eval("b"))\nreturn "OUT " .. r.stdout')
with harness.provider(once(look), price=PRICE) as (url, requests):
    ar(path, url, 0.1, extra=["--yolo"])
outs = [o for o in outputs(requests) if "OUT " in o]
assert outs and all(o.split("OUT ")[1].lstrip().startswith("0") and "secret ledger" not in o and "held-out" not in o for o in outs), outs
assert (grader / "score.sh").read_text().count("\n") == 1, "grader root was modified"
check("--yolo episodes get no credentials and cannot read other runs or touch grader roots")

# 14. Detached processes of agents and evaluators are reaped when their scratch copy is deleted.
spec = 'editable = ["x.txt", "pid.txt"]\neval = "python3 search.py; (setsid sleep 300 >/dev/null 2>&1 & echo $! > /dev/null)"\ntimeout = 30\n'
files = {"x.txt": "0\n", "pid.txt": "", "search.py": 'import json, subprocess, os\np = subprocess.Popen(["python3", "-c", "import os, time; os.setsid(); time.sleep(300)"], start_new_session=True)\nprint(json.dumps({"score": 0, "report": {"evaluator_daemon": p.pid}}))'}
path = workspace("reap", files, spec)
daemon = ('write("d.py", "import os, time\\nif os.fork() == 0:\\n    os.setsid()\\n    open(\'pid.txt\', \'w\').write(str(os.getpid()))\\n    time.sleep(300)\\nos._exit(0)\\n")\n'
          'sh("python3 d.py >/dev/null 2>&1 </dev/null; sleep 0.5")\nreturn ar.eval("daemon")')
with harness.provider(once(daemon), price=PRICE) as (url, requests):
    report = report_of(ar(path, url, 0.1))
snap = ledger(path, report)[0]["snap"]
pids = [int(base64.b64decode(json.loads((path / ".harness/ar" / report["run"] / "snaps" / f"{snap}.json").read_text())["pid.txt"]))]
pids += [int(m) for o in outputs(requests) for m in re.findall(r"evaluator_daemon=(\d+)", o)]
assert len(pids) >= 2, (pids, outputs(requests))
time.sleep(0.5)
alive = [p for p in pids if subprocess.run(["kill", "-0", str(p)], capture_output=True).returncode == 0]
assert not alive, f"detached processes survived: {alive}"
check("detached agent and evaluator processes are reaped")

# 15. Snapshot limits: too many files or too deep a tree is refused per eval, never fatal to the run.
files = {"pkg/a.txt": "0\n", "search.py": 'import json; print(json.dumps({"score": 1}))'}
path = workspace("limits", files, 'editable = ["pkg"]\neval = "python3 search.py"\ntimeout = 30\n')
code = ('sh("mkdir -p pkg/many && cd pkg/many && seq 1 10001 | xargs touch")\nlocal ok1, e1 = pcall(ar.eval, "many")\n'
        'sh("rm -rf pkg/many; mkdir -p pkg/$(printf \'d/%.0s\' $(seq 1 40))")\nlocal ok2, e2 = pcall(ar.eval, "deep")\n'
        'sh("rm -rf pkg/d")\nprint(ar.eval("fine"))\nreturn "E1 " .. tostring(e1) .. " E2 " .. tostring(e2)')
with harness.provider(once(code), price=PRICE) as (url, requests):
    report = report_of(ar(path, url, 0.1))
outs = " ".join(outputs(requests))
assert "more than 10000 files" in outs and "deeper than 32" in outs and report["evals"] >= 1, outs[-2000:]
check("snapshot file-count and depth limits refuse one eval, not the run")

# 16. The frozen base is the committed tree byte for byte: export attributes do not apply.
files = {"x.txt": "0\n", ".gitattributes": "hidden.txt export-ignore\nsubst.txt export-subst\n", "hidden.txt": "here\n",
         "subst.txt": "$Format:%H$\n",
         "search.py": 'import json\nok = open("hidden.txt").read() == "here\\n" and open("subst.txt").read() == "$Format:%H$\\n"\nprint(json.dumps({"score": 1 if ok else 0}))'}
path = workspace("attributes", files, 'editable = ["x.txt"]\neval = "python3 search.py"\ntimeout = 30\n')
with harness.provider(once('return ar.eval("b")'), price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.1))
assert ledger(path, report)[0]["score"] == 1, ledger(path, report)
check("the frozen base ignores export attributes")

# 16b. A deterministic evaluator scores each state once; ar.reset() restores the best state.
code = ('print("A", ar.eval("baseline").cached)\nwrite("x.txt", "6\\n")\nprint("B", ar.eval("six").improved)\n'
        'write("x.txt", "1\\n")\nprint("C", ar.eval("one").improved)\nprint("R", ar.reset().best)\n'
        'print("X", read("x.txt"))\nlocal r = ar.eval("six again")\nreturn "D " .. tostring(r.cached) .. " " .. r.n')
path = workspace("reset")
with harness.provider(once(code, ("r1-w1",)), price=PRICE) as (url, requests):
    report = report_of(ar(path, url, 0.3))  # room for r1-w1's claim judgements and its reply
out = " ".join([o for o in outputs(requests) if "D " in o][0].split())
assert "A nil" in out and "B true" in out and "C false" in out and "R -1" in out and "X 6" in out and "D true 2" in out, out
rows = ledger(path, report)
assert [r["note"] for r in rows[:3]] == ["baseline", "six", "one"] and not any(r["note"] == "six again" for r in rows), rows
check("re-scoring a scored state returns the stored result without a ledger row, and ar.reset restores the best")

# 16c. ar.reset never writes or deletes through a symlink the agent planted in an editable path's parent.
outside = BASE / "outside"
outside.mkdir()
(outside / "x.txt").write_text("keep\n")
code = (f'print(ar.eval("baseline"))\nsh("rm -rf pkg && ln -s {outside} pkg")\n'
        'local ok, e = pcall(ar.reset)\nreturn "RESET " .. tostring(ok) .. " " .. tostring(e)')
path = workspace("reset-link", {"pkg/x.txt": "0\n", "search.py": 'import json; print(json.dumps({"score": 1}))'},
                 'editable = ["pkg/x.txt"]\neval = "python3 search.py"\ntimeout = 30\n')
with harness.provider(once(code, ("r1-w1",)), price=PRICE) as (url, requests):
    ar(path, url, 0.3)
out = [o for o in outputs(requests) if "RESET " in o][0]
assert "RESET false" in out and (outside / "x.txt").read_text() == "keep\n", (out, list(outside.iterdir()))
check("ar.reset refuses a symlinked parent and leaves the link target untouched")

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

print(f"{passed}/{passed} ar checks passed")
