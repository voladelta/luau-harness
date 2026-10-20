#!/usr/bin/env python3
"""Offline autoresearch (`hx run --ar`) checks with the real CLI, a scripted provider and a toy evaluator.

The toy task: x.txt holds an integer; the search evaluator scores -(x-7)^2 and the held-out one -(x-5)^2, so
selection must prefer held-out quality over the agent-visible score. The isolation checks replay attacks an
independent review found against the first design: each must now leave the recorded scores honest.
"""
import base64
import json
import os
import signal
import subprocess
import time
import redesign_checks as harness

BASE = harness.BASE / "ar"
BASE.mkdir(exist_ok=True)
PRICE = {"prompt": "0.00001", "completion": "0.00001"}
SCORE = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": -(x - %d) ** 2, "report": {"x": x}}))'
SINGLE = {"HX_AR_SCHEDULE": "single"}
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


def once(code):
    """A provider whose agent runs `code` once per child and then finishes."""
    def reply(body, index):
        return "done" if answered(body) else {"code": code}
    return reply


def outputs(requests):
    return [item.get("output", "") for body in requests for item in body.get("input", []) if item.get("type") == "function_call_output"]


# 1. The full schedule: 3 rounds x 4 branches plus leader segments, host-owned best states, held-out selection.
counter = [0]


def explorer(body, index):
    if answered(body):
        return "done"
    counter[0] += 1
    value = (counter[0] * 7) % 11  # deterministic spread over 0..10
    return {"code": f'write("x.txt", "{value}\\n")\nreturn ar.eval("try {value}")'}


path = workspace("schedule")
with harness.provider(explorer, price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.1))
rows = ledger(path, report)
assert report["cost_usd"] <= 0.1 + 1e-9, report
assert report["evals"] == len(rows) and len(rows) >= 15, (report["evals"], len(rows))
assert all(r["valid"] for r in rows), rows
assert {f"r{r}b{k}" for r in range(3) for k in range(4)} <= {r["lineage"] for r in rows}
check("schedule runs every fork branch within budget")

assert len(report["selections"]) == 4, report["selections"]
final = report["selections"][-1]
best_select = max(c["select"] for c in final["candidates"] if c["select"] is not None)
assert report["select"] == best_select, (report["select"], best_select)
x = int((path / "x.txt").read_text())
assert -(x - 5) ** 2 == best_select, (x, best_select)
check("final state is the held-out winner and is restored into the workspace")

for selection in report["selections"][:3]:
    scored = [c for c in selection["candidates"] if c["select"] is not None]
    winner = next(c for c in scored if c["lineage"] == selection["winner"])
    assert winner["select"] == max(c["select"] for c in scored), selection
check("each round's winner maximizes the held-out score")

task = (path / ".harness/ar" / report["run"] / "episodes/r1b1.1.md").read_text()
r0_winner = report["selections"][0]["winner"]
assert all(f"\n{r['n']} |" not in task for r in rows if r["lineage"] == "r1b0"), "sibling rows leaked into another branch"
assert any(f"\n{r['n']} |" in task for r in rows if r["lineage"] == r0_winner), "parent rows missing"
check("branches inherit the parent ledger and not their siblings'")

for r in rows:
    snap = json.loads((path / ".harness/ar" / report["run"] / "snaps" / f"{r['snap']}.json").read_text())
    value = int(base64.b64decode(snap["x.txt"]))
    assert r["score"] == -(value - 7) ** 2, (r, value)
check("every ledger score is the score of its recorded snapshot")

# 2. Tampering outside the editable paths never reaches the evaluator: no Git hooks, no ignore rules, no evaluator edits.
tamper = (f'sh("git init -q; git config core.fsmonitor $PWD/hook.sh; printf \'#!/bin/sh\\\\ntouch {BASE}/pwned\\\\n\' > hook.sh; chmod +x hook.sh; echo json.py >> .git/info/exclude")\n'
          'write("search.py", "print(\'{\\"score\\": 1000}\')")\n'
          'write("json.py", "def dumps(o): return \'{\\"score\\": 999}\'\\n")\n'
          'return ar.eval("tamper")')
for mode in ("sandbox", "yolo"):
    path = workspace(f"tamper-{mode}")
    with harness.provider(once(tamper), price=PRICE) as (url, _):
        report = report_of(ar(path, url, 0.01, env=SINGLE, extra=["--yolo"] if mode == "yolo" else []))
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
    ar(path, url, 0.01, env=SINGLE)
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
    with harness.provider(once(look), price=PRICE) as (url, requests):
        ar(path, url, 0.03, env=SINGLE, extra=["--yolo"] if mode == "yolo" else [])
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
    report = report_of(ar(path, url, 0.01, env=SINGLE))
first = ledger(path, report)[0]
snap = json.loads((path / ".harness/ar" / report["run"] / "snaps" / f"{first['snap']}.json").read_text())
assert base64.b64decode(snap["x.txt"]) == b"0\n" and first["score"] == -49, (first, snap)
check("snapshot and score always agree")

# 6. Binary files and symlinks in an editable directory are carried or skipped, never fatal.
files = {"pkg/a.txt": "0\n", "search.py": 'import json; print(json.dumps({"score": 1}))'}
path = workspace("binary", files, 'editable = ["pkg"]\neval = "python3 search.py"\ntimeout = 30\n')
junk = 'sh("printf \'\\\\377\\\\376\' > pkg/blob.bin; ln -s /etc/hosts pkg/link")\nreturn ar.eval("junk")'
with harness.provider(once(junk), price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.02, env=SINGLE))
assert report["evals"] >= 1 and (path / "pkg/blob.bin").read_bytes() == b"\xff\xfe" and not (path / "pkg/link").exists()
check("binary editable files survive and symlinks are skipped")

# 7. A killed search resumes from its journal; the charge for the interrupted attempt is journaled durably.
path = workspace("resume")
counter[0] = 0
with harness.provider(explorer, price=PRICE) as (url, _):
    process = subprocess.Popen(command(path, url, 0.1), env=harness.ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=True)
    deadline, journal = time.time() + 120, None
    while time.time() < deadline:
        journals = list((path / ".harness/ar").glob("*/journal.jsonl"))
        if journals:
            lines = journals[0].read_text().splitlines()
            if any('"chosen"' in l for l in lines) and '"started"' in lines[-1]:
                journal = journals[0]
                break
        time.sleep(0.01)
    assert journal, "never caught an attempt in flight after the first selection"
    os.killpg(process.pid, signal.SIGKILL)
    process.wait()
    result = subprocess.run([str(harness.HX), "run", "--ar", "--resume", str(journal.parent), "--workspace", str(path)],
                            env=harness.ENV, capture_output=True, text=True, timeout=600)
    assert result.returncode == 0, result.stderr[-3000:]
report = report_of(result)
records = [json.loads(l) for l in journal.read_text().splitlines()]
assert any(r["kind"] == "finished" and r.get("interrupted") for r in records), "interrupted charge not journaled"
assert [s["round"] for s in report["selections"]] == [0, 1, 2, 3] and report["cost_usd"] <= 0.1 + 1e-9
assert "resumed" in result.stderr
check("killed search resumes and journals the interrupted charge")

# 8. The single-run baseline: one lineage with the whole budget.
path = workspace("single")
counter[0] = 0
with harness.provider(explorer, price=PRICE) as (url, _):
    report = report_of(ar(path, url, 0.05, env=SINGLE))
assert report["schedule"] == "single" and {r["lineage"] for r in ledger(path, report)} == {"s0"}, report
assert len(report["selections"]) == 1 and report["cost_usd"] <= 0.05 + 1e-9, report
check("single schedule runs one lineage on the whole budget")


# 9. Provider crashes are charged from the child's journal and retried; misconfiguration fails fast and nonzero.
def limited(body, index):
    return {"status": 429, "error": "rate limited"} if index < 3 else explorer(body, index)


path = workspace("crash")
with harness.provider(limited, price=PRICE) as (url, _):
    result = ar(path, url, 0.05, env={**SINGLE, "HX_AR_RETRY_SECS": "0"})
report = report_of(result)
assert "retrying" in result.stderr and report["evals"] > 0, result.stderr[-1500:]
records = [json.loads(l) for l in (path / ".harness/ar" / report["run"] / "journal.jsonl").read_text().splitlines()]
crashed = next(r for r in records if r["kind"] == "finished" and r["evals"] == 0)
assert crashed["cost"] < 0.01, crashed
path = workspace("misconfigured")
start = time.time()
with harness.provider(explorer, price=PRICE) as (url, _):
    result = ar(path, url, 0.05, ok=False, env={**SINGLE, "HX_AR_RETRY_SECS": "0"}, key="HX_AR_NO_SUCH_KEY")
assert result.returncode != 0 and "keep failing" in result.stderr and time.time() - start < 120, result.stderr[-1500:]
check("crashes are charged from child journals and retried; misconfiguration fails fast")

# 10. A broken select evaluator falls back to the search score, visibly.
path = workspace("fallback", spec='editable = ["x.txt"]\neval = "python3 search.py"\nselect = "exit 3"\ntimeout = 30\n')
counter[0] = 0
with harness.provider(explorer, price=PRICE) as (url, _):
    result = ar(path, url, 0.03, env=SINGLE)
report = report_of(result)
assert report["selections"][0]["fallback"] is True and "held-out" in result.stderr, report
check("an unusable select evaluator falls back to search, recorded")

# 11. Launch validation.
path = workspace("flags")
base_cmd = [str(harness.HX), "run", "--ar", "--workspace", str(path), "--prompt-file", str(path / "program.md"), "--model", "mock"]
result = subprocess.run(base_cmd, env=harness.ENV, capture_output=True, text=True)
assert result.returncode != 0 and "--max-cost" in result.stderr, result.stderr
(path / "notes.txt").write_text("uncommitted")
result = subprocess.run(base_cmd + ["--max-cost", "1"], env=harness.ENV, capture_output=True, text=True)
assert result.returncode != 0 and "notes.txt" in result.stderr, result.stderr
(path / "notes.txt").unlink()
(path / ".harness/ar.toml").write_text('editable = ["."]\neval = "true"\n')
result = subprocess.run(base_cmd + ["--max-cost", "1"], env=harness.ENV, capture_output=True, text=True)
assert result.returncode != 0 and "editable path" in result.stderr, result.stderr
check("launch requires a budget, a clean scope and sane editable paths")

print(f"{passed}/{passed} ar checks passed")
