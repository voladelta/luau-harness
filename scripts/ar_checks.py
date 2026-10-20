#!/usr/bin/env python3
"""Offline autoresearch (`hx run --ar`) checks with the real CLI, a scripted provider and a toy evaluator.

The toy task: x.txt holds an integer; the search evaluator scores -(x-7)^2 and the held-out one -(x-5)^2, so
selection must prefer held-out quality over the agent-visible score.
"""
import json
import subprocess
import redesign_checks as harness

BASE = harness.BASE / "ar"
BASE.mkdir(exist_ok=True)
PRICE = {"prompt": "0.00001", "completion": "0.00001"}
passed = 0


def check(name):
    global passed
    passed += 1
    print(f"ok {passed} {name}")


def workspace(name, select=True):
    path = BASE / name
    path.mkdir(parents=True)
    git = lambda *a: subprocess.run(["git", *a], cwd=path, check=True, capture_output=True)
    git("init", "-q")
    git("config", "user.email", "test@test.local")
    git("config", "user.name", "test")
    (path / ".harness").mkdir()
    (path / ".harness/init.luau").write_text('require("std/recheck").enabled = false\n')
    (path / ".gitignore").write_text(".harness/\n")
    (path / "x.txt").write_text("0\n")
    score = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": -(x - %d) ** 2, "report": {"x": x}}))'
    (path / "search.py").write_text(score % 7)
    (path / "select.py").write_text(score % 5)
    spec = 'editable = ["x.txt"]\neval = "python3 search.py"\ntimeout = 30\n'
    if select:
        spec += 'select = "python3 select.py"\n'
    (path / ".harness/ar.toml").write_text(spec)
    (path / "program.md").write_text("Make the score as high as possible.")
    git("add", "-A")
    git("commit", "-qm", "init")
    return path


def ar(path, url, budget, ok=True):
    result = subprocess.run([str(harness.HX), "run", "--ar", "--workspace", str(path), "--prompt-file", str(path / "program.md"),
                             "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY", "--yolo",
                             "--max-cost", str(budget)], env=harness.ENV, capture_output=True, text=True, timeout=600)
    (BASE / f"{path.name}.stderr").write_text(result.stderr)
    if ok:
        assert result.returncode == 0, (result.returncode, result.stdout[-2000:], result.stderr[-4000:])
    return result


def ledger(path, report):
    return [json.loads(line) for line in (path / ".harness/ar" / report["run"] / "ledger.jsonl").read_text().splitlines()]


def answered(body):
    return "function_call_output" in json.dumps(body) or '"role": "tool"' in json.dumps(body)


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
    result = ar(path, url, 0.2)
report = json.loads(result.stdout.strip().splitlines()[-1])
rows = ledger(path, report)
assert report["cost_usd"] <= 0.2 + 1e-9, report
assert report["evals"] == len(rows) and len(rows) >= 15, (report["evals"], len(rows))
assert all(r["valid"] for r in rows), rows
lineages = {r["lineage"] for r in rows}
assert {f"r{r}b{k}" for r in range(3) for k in range(4)} <= lineages, lineages
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

# Forked branches see their ancestors' ledger up to the fork, never a sibling's rows.
task = (path / ".harness/ar" / report["run"] / "episodes/r1b1.1.md").read_text()
r0_winner = report["selections"][0]["winner"]
siblings = [r for r in rows if r["lineage"] == "r1b0"]
assert any(f"| try " in line for line in task.splitlines()), task
assert all(f"\n{r['n']} |" not in task for r in siblings), "sibling rows leaked into another branch"
assert any(f"\n{r['n']} |" in task for r in rows if r["lineage"] == r0_winner), "parent rows missing"
check("branches inherit the parent ledger and not their siblings'")

# 2. Changes outside the editable paths are refused before evaluation.
def trespasser(body, index):
    if answered(body):
        return "done"
    return {"code": 'write("search.py", "print(1)")\nreturn ar.eval("tamper")'}


path = workspace("scope", select=False)
with harness.provider(trespasser, price=PRICE) as (url, _):
    result = ar(path, url, 0.05)
report = json.loads(result.stdout.strip().splitlines()[-1])
assert report["evals"] == 0, report
logs = "".join(p.read_text() for p in (path / ".harness/log").glob("*.jsonl"))
assert "only the editable paths" in logs and "search.py" in logs, "scope refusal missing from journals"
check("evaluation refuses changes outside the editable paths")

# 3. Launch validation.
path = workspace("flags", select=False)
result = subprocess.run([str(harness.HX), "run", "--ar", "--workspace", str(path), "--prompt-file", str(path / "program.md"),
                         "--model", "mock"], env=harness.ENV, capture_output=True, text=True)
assert result.returncode != 0 and "--max-cost" in result.stderr, result.stderr
(path / "notes.txt").write_text("uncommitted")
result = subprocess.run([str(harness.HX), "run", "--ar", "--workspace", str(path), "--prompt-file", str(path / "program.md"),
                         "--model", "mock", "--max-cost", "1"], env=harness.ENV, capture_output=True, text=True)
assert result.returncode != 0 and "notes.txt" in result.stderr, result.stderr
check("launch requires a budget and a clean scope")

print(f"{passed}/{passed} ar checks passed")
