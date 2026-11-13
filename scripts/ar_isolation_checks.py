#!/usr/bin/env python3
"""Offline isolation checks for autoresearch (`hx run --ar`): the real CLI, a scripted provider, toy evaluators.

Each scenario replays an attack or defect an independent review found (`leak`, `mode`, `ideas` and the rest): candidate
code that leaves a daemon holding held-out data in its argv, an evaluator that dumps its grader on failure, an
executable editable file, ideas planned one at a time, eval copies left behind by a stopped run. Every evaluator and
episode shell command must run in fresh namespaces (or the cgroup fallback), so none of these reaches a later process.
"""
import json
import os
import signal
import stat
import subprocess
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import redesign_checks as harness

BASE = harness.BASE / "ar-isolation"
BASE.mkdir(exist_ok=True)
SCORE = 'import json; x = int(open("x.txt").read()); print(json.dumps({"score": -(x - %d) ** 2}))'
passed = 0
lock = threading.Lock()


def check(name):
    global passed
    with lock:
        passed += 1
        print(f"ok {passed} {name}", flush=True)


def git(path, *args):
    subprocess.run(["git", *args], cwd=path, check=True, capture_output=True)


def workspace(name, files=None, spec=None, modes=None, links=None):
    path = BASE / name
    path.mkdir(parents=True)
    git(path, "init", "-q")
    git(path, "config", "user.email", "test@test.local")
    git(path, "config", "user.name", "test")
    (path / ".harness").mkdir()
    (path / ".gitignore").write_text(".harness/\n")
    for rel, text in (files or {"x.txt": "0\n", "search.py": SCORE % 7, "select.py": SCORE % 5}).items():
        (path / rel).parent.mkdir(parents=True, exist_ok=True)
        (path / rel).write_text(text)
    for rel, mode in (modes or {}).items():
        (path / rel).chmod(mode)
    for rel, target in (links or {}).items():
        os.symlink(target, path / rel)
    (path / ".harness/ar.toml").write_text(spec or 'editable = ["x.txt"]\neval = "python3 search.py"\nselect = "python3 select.py"\ntimeout = 30\n')
    (path / "program.md").write_text("Make the score as high as possible.")
    git(path, "add", "-A")
    git(path, "commit", "-qm", "init")
    return path


def command(path, url):
    return [str(harness.HX), "run", "--ar", "--workspace", str(path), "--prompt-file", str(path / "program.md"),
            "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY"]


def ar(path, url, ok=True, env=None):
    result = subprocess.run(command(path, url), env={**harness.ENV, **(env or {})}, capture_output=True, text=True, timeout=300)
    (BASE / f"{path.name}.stderr").write_text(result.stderr)
    if ok:
        assert result.returncode == 0, (result.returncode, result.stderr[-3000:])
    return result


def run_dir(path):
    return sorted((harness.state_dir(path) / "ar").glob("*/journal.jsonl"))[-1].parent


def records_of(run):
    return [json.loads(l) for l in (run / "journal.jsonl").read_text().splitlines() if l.endswith("}")]


def text_of(body):
    out = []
    def walk(v):
        if isinstance(v, str): out.append(v)
        elif isinstance(v, dict): [walk(x) for x in v.values()]
        elif isinstance(v, list): [walk(x) for x in v]
    walk(body)
    return "\n".join(out)


def answered(body):
    return "function_call_output" in json.dumps(body)


def role_of(body):
    t = text_of(body)
    if "You compare research ideas" in t: return "dedupe", None
    for role, marker in (("idea", "## Autoresearch idea for "), ("research", "## Autoresearch research lead ("), ("explore", "## Autoresearch explore worker "),
                         ("exploit", "## Autoresearch exploit worker "), ("audit", "## Autoresearch auditor ")):
        if marker in t:
            return role, t.split(marker, 1)[1].split(None, 1)[0].rstrip(":)")
    return "other", None


def outputs(requests):
    return [item.get("output", "") for body in requests for item in body.get("input", []) if item.get("type") == "function_call_output"]


def provider(reply):
    """A scripted endpoint: idea calls get an idea of their own per worker, dedupe calls find none the same, every other
    request goes to `reply`."""
    def wrapped(body, index):
        role, label = role_of(body)
        if role == "idea": return f"IDEA: the idea planned for worker {label}"
        if role == "dedupe": return "Duplicates: none"
        return reply(body, index)
    return harness.provider(wrapped)


def running(prefix):
    """Processes on this machine whose argv[0] starts with `prefix`."""
    found = []
    for p in Path("/proc").iterdir():
        try:
            if p.name.isdigit() and (p / "cmdline").read_bytes().startswith(prefix.encode()): found.append(int(p.name))
        except OSError:
            pass
    return found


def scenario_leak():
    # Candidate code under the select evaluator daemonizes with the held-out secret as its argv[0]; the next audit's
    # shell looks for it in /proc. Another candidate prints a grader file and exits nonzero under the search evaluator.
    grader, holdout = BASE / "leak-grader", BASE / "leak-holdout"
    grader.mkdir()
    holdout.mkdir()
    (holdout / "secret.txt").write_text("HELDOUT-ANSWER-42\n")
    (grader / "search-secret.txt").write_text("GRADER-SECRET-7\n")
    (grader / "search.py").write_text('import json, sys\nsys.path.insert(0, ".")\nimport cand\ncand.dump(sys.argv[1])\nprint(json.dumps({"score": cand.VALUE}))\n')
    (grader / "select.py").write_text('import sys, json\nsys.path.insert(0, ".")\nimport cand\ncand.run(sys.argv[1] + "/secret.txt")\nprint(json.dumps({"score": 1}))\n')
    files = {"cand.py": "VALUE = 0\ndef run(path): return 0\ndef dump(path): return 0\n"}
    spec = (f'editable = ["cand.py"]\neval = "python3 {grader}/search.py {grader}"\nselect = "python3 {grader}/select.py {holdout}"\n'
            f'grader = ["{grader}"]\nholdout = ["{holdout}"]\ntimeout = 30\n')
    path = workspace("leak", files, spec)
    leaker = ('VALUE = 1\ndef dump(path): return 0\ndef run(path):\n    import os\n    data = open(path).read().strip()\n'
              '    if os.fork() == 0:\n        os.setsid()\n        if os.fork() == 0:\n            os.chdir("/")\n'
              '            fd = os.open("/dev/null", os.O_RDWR)\n            for i in (0, 1, 2): os.dup2(fd, i)\n'
              '            os.execv("/bin/sleep", ["LEAK:" + data, "90"])\n        os._exit(0)\n    return 0\n')
    dumper = ('VALUE = 2\ndef run(path): return 0\ndef dump(root):\n    import sys\n    print(open(root + "/search-secret.txt").read())\n'
              '    print(open(root + "/search-secret.txt").read(), file=sys.stderr)\n    sys.exit(3)\n')
    snoop = 'local r = sh("cat /proc/[0-9]*/cmdline 2>/dev/null | tr \'\\\\0\' \' \' | grep -ao \'L[E]AK:[A-Z0-9-]*\' | head -1")\nprint("SEEN " .. r.stdout .. "|")\n'
    def reply(body, index):
        role, label = role_of(body)
        if role == "research": return "BRIEF"
        if answered(body): return "FINDING" if role != "audit" else "done"
        if label == "r1-w1":
            return {"code": f'write("cand.py", {json.dumps(leaker)})\nreturn ar.eval("v=1")'}
        if label == "r1-w2":
            return {"code": f'write("cand.py", {json.dumps(dumper)})\nlocal r = ar.eval("v=2")\nprint("DUMP " .. tostring(r.report.error))\nreturn "x"'}
        if role in ("audit", "explore", "exploit"):
            return {"code": snoop + 'return "x"'}
        return "done"
    try:
        with provider(reply) as (url, requests):
            ar(path, url)
        seen = [o for o in outputs(requests) if "SEEN" in o]
        assert seen and all("LEAK" not in o.split("SEEN", 1)[1] for o in seen), seen
        assert any("SEEN" in o for body in requests if role_of(body)[0] == "audit" for o in outputs([body])), "no audit looked"
        assert not running("LEAK:"), "a daemon of the select evaluator survived it"
        dumps = [o for o in outputs(requests) if "DUMP" in o]
        assert dumps and all("GRADER-SECRET" not in o and "status 3" in o for o in dumps), dumps
        failures = list((run_dir(path) / "failures").glob("*.txt"))
        assert any("GRADER-SECRET" in f.read_text() for f in failures), "the owner keeps a failed evaluator's output"
        transcript = json.dumps(requests)
        assert "HELDOUT-ANSWER-42" not in transcript and "GRADER-SECRET-7" not in transcript
    finally:
        for pid in running("LEAK:"):
            os.kill(pid, signal.SIGKILL)
    check("nothing candidate code leaves under an evaluator reaches a later shell, and a failed evaluator's output stays with the owner")


def scenario_mode():
    # An executable editable file keeps its mode in every copy and in the owner's workspace, and a symlink the
    # snapshots skip survives the result being written back.
    spec = 'editable = ["run.sh", "pkg"]\neval = "./run.sh"\ntimeout = 30\n'
    files = {"run.sh": '#!/bin/sh\necho "{\\"score\\": $(cat pkg/n.txt)}"\n', "pkg/n.txt": "1\n", "notes.txt": "n"}
    path = workspace("mode", files, spec, modes={"run.sh": 0o755}, links={"pkg/link": "../notes.txt"})
    code = 'write("pkg/n.txt", "2\\n")\nlocal r = ar.eval("two")\nprint("MODE " .. tostring(r.valid) .. " " .. tostring(r.score))\nreturn "x"'
    def reply(body, index):
        role, label = role_of(body)
        if role == "audit": return "ok\nVERDICT: PASS"
        if answered(body) or role == "research": return "done"
        return {"code": code} if label == "r1-w1" else "done"
    with provider(reply) as (url, requests):
        result = ar(path, url)
    assert "seed scores 1" in result.stderr, result.stderr[-2000:]
    assert any("MODE true 2" in o for o in outputs(requests)), outputs(requests)
    assert stat.S_IMODE((path / "run.sh").stat().st_mode) == 0o755
    assert (path / "pkg/link").is_symlink() and os.readlink(path / "pkg/link") == "../notes.txt"
    run = run_dir(path)
    snaps = {r["snap"] for r in records_of(run) if r["kind"] == "score"}
    manifests = [json.loads((run / "blobs" / snap).read_bytes()) for snap in snaps]
    assert len(manifests) == 2 and all(m["run.sh"]["mode"] == 0o755 and "pkg/link" not in m for m in manifests), manifests
    assert all(isinstance(e, dict) and set(e) == {"mode", "blob"} for m in manifests for e in m.values())
    blobs = {e["blob"] for m in manifests for e in m.values()}
    assert len(blobs) == 3 and blobs <= {p.name for p in (run / "blobs").iterdir()}, "each file's bytes are stored once"
    check("snapshots keep file modes and share blobs; writing the result back keeps the modes and skipped symlinks")


def scenario_ideas():
    # Eight workers' ideas are planned before round 1, each idea call taking two seconds: sampled side by side, the calls
    # arrive together instead of two seconds apart, and every worker then builds the idea planned for it.
    path = workspace("ideas")
    times, built = [], []
    def reply(body, index):
        role, label = role_of(body)
        if role == "idea":
            times.append(time.time())
            time.sleep(2.0)
            return f"IDEA: idea number {label.split('-w')[1]}"
        if role == "dedupe": return "Duplicates: none"
        if answered(body) or role == "research": return "done" if role != "research" else "BRIEF"
        if role == "explore":
            built.append((label, text_of(body).split("<idea>")[1].split("</idea>")[0].strip()))
            return {"code": f'write("x.txt", "{label[-1]}\\n")\nreturn ar.eval("x")'}
        return "done"
    with harness.provider(reply) as (url, requests):
        p = subprocess.Popen(command(path, url), env=harness.ENV, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        deadline = time.time() + 120
        while len(built) < 8 and time.time() < deadline: time.sleep(0.1)
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()
    ts = sorted(times)
    assert len(ts) >= 8, len(ts)
    spread = ts[7] - ts[0]
    assert spread < 3, f"idea calls waited on each other: {[round(t - ts[0], 1) for t in ts]}"
    assert sorted(built) == [(f"r1-w{w}", f"idea number {w}") for w in range(1, 9)], built
    check("ideas are planned side by side before a round, and each worker builds the idea planned for it")


def scenario_verdict():
    # ar.verdict exists only for auditors, needs a reason to fail, is written whole and is final.
    path = workspace("verdict")
    def reply(body, index):
        role, label = role_of(body)
        if answered(body) or role == "research": return "done"
        if label == "r1-w1":
            return {"code": 'print("WORKER " .. tostring(ar.verdict))\nwrite("x.txt", "6\\n")\nreturn ar.eval("six")'}
        if role == "audit":
            return {"code": 'print("NOREASON", pcall(ar.verdict, {pass = false}))\nprint("FIRST", pcall(ar.verdict, {pass = true, reason = "earned"}))\n'
                            'print("SECOND", pcall(ar.verdict, {pass = false, reason = "changed my mind"}))\nreturn "x"'}
        return "done"
    with provider(reply) as (url, requests):
        ar(path, url)
    outs = outputs(requests)
    assert any("WORKER nil" in o for o in outs), outs
    assert any("NOREASON\tfalse" in o and "needs its reason" in o for o in outs), outs
    assert any("FIRST\ttrue" in o for o in outs) and any("SECOND\tfalse" in o and "final" in o for o in outs), outs
    verdicts = [{k: r[k] for k in ("pass", "reason")} for r in records_of(run_dir(path)) if r["kind"] == "verdict"]
    assert verdicts == [{"pass": True, "reason": "earned"}], verdicts
    check("ar.verdict is for auditors only, needs a reason to fail, and is recorded once")


def scenario_sigterm():
    # A run stopped with SIGTERM while a worker's evaluation runs leaves no scratch or evaluation copy behind, and the
    # evaluation's processes die with it.
    tmp = Path(tempfile.mkdtemp(prefix="tmp-", dir=BASE))
    marker = f"hx-iso-slow-{os.getpid()}"
    files = {"x.txt": "0\n", "search.py": f'import json, subprocess\nx = int(open("x.txt").read())\nif x == 9: subprocess.run(["bash", "-c", "exec -a {marker} sleep 60"])\nprint(json.dumps({{"score": x}}))'}
    path = workspace("sigterm", files, 'editable = ["x.txt"]\neval = "python3 search.py"\ntimeout = 120\n')
    def reply(body, index):
        role, label = role_of(body)
        if answered(body) or role == "research": return "done"
        if label == "r1-w1": return {"code": 'write("x.txt", "9\\n")\nreturn ar.eval("nine")'}
        return "done"
    with provider(reply) as (url, _):
        p = subprocess.Popen(command(path, url), env={**harness.ENV, "TMPDIR": str(tmp)}, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL, start_new_session=True)
        deadline = time.time() + 90
        while not running(marker) and time.time() < deadline: time.sleep(0.1)
        assert running(marker), "the slow evaluation never started"
        assert list(tmp.glob("hx-ar-eval-*")) and list(tmp.glob("hx-ar-ws-*")), "the orchestrator's evaluation and episode copies"
        os.killpg(p.pid, signal.SIGTERM) # what the docs advise: the whole group, so the evaluator gets it too
        p.wait(timeout=30)
        # The children stop on their own SIGTERM, a moment after the orchestrator.
        deadline = time.time() + 15
        while (running(marker) or any(e.name.startswith("hx-ar-") for e in tmp.iterdir())) and time.time() < deadline: time.sleep(0.1)
    left = [e.name for e in tmp.iterdir() if e.name.startswith("hx-ar-")]
    assert not left, f"copies left behind: {left}"
    # The evaluation the shutdown killed is not a result: no score says the state is invalid, and no eval names it.
    records = records_of(run_dir(path))
    assert not [r for r in records if r["kind"] == "score" and not r["valid"]], records
    assert not [r for r in records if r["kind"] == "eval"], records
    assert not running(marker), "the evaluation's processes outlived the run"
    check("SIGTERM leaves no episode or evaluation copy and no evaluation process behind")


def scenario_unavailable():
    # Where children cannot be isolated, the run refuses at the first evaluation and says why.
    path = workspace("unavailable")
    with provider(lambda body, index: "done") as (url, _):
        result = ar(path, url, ok=False, env={"HX_TEST_ISOLATION": "none"})
    assert result.returncode != 0 and "cannot isolate" in result.stderr, result.stderr[-2000:]
    check("without namespaces or a cgroup to fall back on, autoresearch refuses to run evaluators")


def scenario_cgroup():
    # The cgroup fallback (no user namespaces): the select evaluator's daemon dies with it all the same.
    marker = f"hx-iso-cg-{os.getpid()}"
    files = {"x.txt": "0\n", "search.py": SCORE % 7,
             "select.py": f'import json, subprocess\nsubprocess.Popen(["bash", "-c", "exec -a {marker} sleep 60"], start_new_session=True)\nx = int(open("x.txt").read())\nprint(json.dumps({{"score": -(x - 5) ** 2}}))'}
    path = workspace("cgroup", files)
    def reply(body, index):
        role, label = role_of(body)
        if role == "audit": return "ok\nVERDICT: PASS"
        if answered(body) or role == "research": return "done"
        if label == "r1-w1": return {"code": 'write("x.txt", "6\\n")\nreturn ar.eval("six")'}
        return "done"
    with provider(reply) as (url, _):
        result = ar(path, url, ok=False, env={"HX_TEST_ISOLATION": "cgroup"})
    if result.returncode != 0 and "cannot isolate" in result.stderr:
        print("skip cgroup fallback: this cgroup cannot hold a child", flush=True)
        return
    assert result.returncode == 0, result.stderr[-2000:]
    time.sleep(0.3)
    assert not running(marker), "a select evaluator's daemon outlived its cgroup"
    check("under the cgroup fallback, an evaluator's daemon dies with it")


with ThreadPoolExecutor(max_workers=4) as pool:
    for future in [pool.submit(f) for f in [v for k, v in sorted(globals().items()) if k.startswith("scenario_")]]:
        future.result()
print(f"{passed}/{passed} ar isolation checks passed")
