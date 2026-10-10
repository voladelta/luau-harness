#!/usr/bin/env python3
"""AC6 crash test: kill hx after EVERY record of a scripted multi-step run, resume from the log, and check that
no side effect is ever repeated and the session state is rebuilt. No API key needed (scripted model).
Strong cases (crash not between an intent and its outcome): the resumed run must finish with exactly the baseline's files.
Unknown-outcome cases (crash right after a write-ahead intent): the effect ran at most once, the model is told
`unknown_outcome`, and the log ends with no unresolved intent."""
import json, os, shutil, subprocess, sys, tempfile, time
os.environ["HX_TRUST"] = "1"  # these checks write .harness config on purpose; trust_checks.sh tests trust
import tempfile as _tf; os.environ["XDG_CONFIG_HOME"] = _tf.mkdtemp()
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HX = str(ROOT / "target/release/hx")
PORT = 8780

REPLIES = [
    'x = 41\nwrite("a.txt", "1")\nsh("echo run1 >> counter.txt")\nwrite("rnd.txt", tostring(math.random(1, 1000000)))\nwrite("t.txt", tostring(os.time() > 0))\nprint("chunk1 done")',
    'print(x + 1)\nprint(read("a.txt"))\nlocal hits = search("run1")\nedit("a.txt", "1", "2")\nsh("echo run2 >> counter.txt")\n'
    'local rs = parallel({ function() return sh("echo p1 >> counter.txt").code end, function() return sh("echo p2 >> counter.txt").code end })\n'
    'write("b.txt", tostring(x + 1) .. ":" .. #hits)\nprint(rs[1], rs[2])',
    'write("final.txt", tostring(x) .. ":" .. read("rnd.txt"))\nprint("chunk3 done")',
]
REPLIES = ["```luau\n" + r + "\n```" for r in REPLIES] + ["All done."]

def run(ws, extra, env_extra=None, log=None):
    env = {**os.environ, "OPENROUTER_API_KEY": "x", **(env_extra or {})}
    cmd = [HX, "run", "--workspace", str(ws), "--model", "m", "--api", "openai", "--base-url", f"http://127.0.0.1:{PORT}", "--key-env", "OPENROUTER_API_KEY"] + extra
    return subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=120)

def newws(base):
    ws = Path(tempfile.mkdtemp(prefix="resume-", dir=base)); ws = ws.resolve()
    (ws / "p.txt").write_text("do the scripted work\n")
    return ws

def state(ws):
    out = {}
    for f in ("a.txt", "b.txt", "final.txt", "rnd.txt", "t.txt"):
        out[f] = (ws / f).read_text() if (ws / f).exists() else None
    out["counter"] = sorted((ws / "counter.txt").read_text().split()) if (ws / "counter.txt").exists() else []
    return out

def lastlog(ws):
    return sorted((ws / ".harness/log").glob("*.jsonl"), key=lambda p: p.stat().st_mtime)[-1]

def verify(log):
    r = subprocess.run([HX, "log", "verify", str(log)], capture_output=True, text=True)
    return r.returncode, r.stdout

def main():
    env = {**os.environ, "MOCK_REPLIES": json.dumps(REPLIES), "MOCK_BY_LEN": "1"}
    mock = subprocess.Popen([sys.executable, str(ROOT / "scripts/mock_llm.py"), str(PORT)], env=env)
    time.sleep(1)
    base = tempfile.mkdtemp(prefix="hx-resume-")
    fails, cases = [], 0
    try:
        # baseline
        bws = newws(base)
        r = run(bws, ["--prompt-file", str(bws / "p.txt"), "--max-turns", "10"])
        assert r.returncode == 0, r.stderr
        rep = json.loads(r.stdout.strip().splitlines()[-1])
        want = state(bws)
        assert want["counter"] == ["p1", "p2", "run1", "run2"] and want["final.txt"] == f"41:{want['rnd.txt']}" and want["b.txt"] == "42:1", want
        log = lastlog(bws)
        recs = [json.loads(l) for l in log.read_text().splitlines()]
        n = len(recs)
        print(f"baseline ok: {n} records, {rep['turns']} turns, state {want}")
        for k in range(1, n + 1):
            cases += 1
            ws = newws(base)
            r1 = run(ws, ["--prompt-file", str(ws / "p.txt"), "--max-turns", "10"], {"HX_TEST_CRASH_AFTER_RECORD": str(k)})
            crashed = r1.returncode != 0
            log = lastlog(ws)
            kind = recs[k - 1]["kind"]; tool = recs[k - 1].get("tool")
            unknown_case = kind == "intent"
            pre = [json.loads(l) for l in log.read_text().splitlines() if l.strip().endswith("}")]
            open_intents = len({r["seq"] for r in pre if r["kind"] == "intent"} - {r.get("of") for r in pre if r["kind"] == "outcome"})
            r2 = run(ws, ["--prompt-file", str(ws / "p.txt"), "--max-turns", "10", "--resume", str(log)]) if crashed else r1
            tag = f"crash after record {k} ({kind}{':' + tool if tool else ''})"
            problems = []
            if r2.returncode != 0:
                problems.append(f"resume failed: {r2.stderr[-300:]}")
            else:
                got = state(ws)
                rep2 = json.loads(r2.stdout.strip().splitlines()[-1])
                dup = [m for m in set(got["counter"]) if got["counter"].count(m) > 1]
                if dup: problems.append(f"side effect repeated: {dup}")
                if not rep2.get("finished"): problems.append("run did not finish")
                rc, out = verify(log)
                if rc != 0: problems.append(f"log after resume: rc={rc} {out.strip()[:200]}")
                if not unknown_case:
                    if got != want and not (got["rnd.txt"] != want["rnd.txt"] and k < 6):
                        # random value legitimately differs from the baseline run only if the crash was before it was recorded
                        diff = {f: (got[f], want[f]) for f in got if got[f] != want[f] and f not in ("rnd.txt", "final.txt")}
                        if diff: problems.append(f"state differs from baseline: {diff}")
                    if got["final.txt"] != f"41:{got['rnd.txt']}": problems.append(f"final.txt not consistent with recorded random: {got['final.txt']} vs {got['rnd.txt']}")
                else:
                    if not set(got["counter"]) <= {"p1", "p2", "run1", "run2"}: problems.append("unexpected counter content")
                    resolved = [r for r in map(json.loads, log.read_text().splitlines()) if r.get("unknown_resolved")]
                    if len(resolved) != open_intents or open_intents < 1: problems.append(f"expected {open_intents} unknown_outcome(s) surfaced and closed, saw {len(resolved)}")
            if problems:
                fails.append((tag, problems)); print("FAIL", tag, problems)
            else:
                print("ok  ", tag, "(unknown-outcome case)" if unknown_case else "")
        # a changed harness between crash and resume must fail loudly and must not run anything
        cases += 1
        ws = newws(base)
        run(ws, ["--prompt-file", str(ws / "p.txt"), "--max-turns", "10"], {"HX_TEST_CRASH_AFTER_RECORD": "17"})
        log = lastlog(ws); before = state(ws)
        (ws / ".harness/override/std").mkdir(parents=True, exist_ok=True)
        (ws / ".harness/override/std/system.md").write_text("You are a different agent now.\n")
        r2 = run(ws, ["--prompt-file", str(ws / "p.txt"), "--max-turns", "10", "--resume", str(log)])
        ok = r2.returncode != 0 and "replay diverged" in r2.stderr and state(ws) == before
        print("ok  " if ok else "FAIL", "changed system prompt between crash and resume => 'replay diverged', nothing executed")
        if not ok: fails.append(("diverge", r2.stderr[-300:]))
    finally:
        mock.kill(); shutil.rmtree(base, ignore_errors=True)
    print(f"{cases - len(fails)} passed, {len(fails)} failed of {cases} crash points")
    sys.exit(1 if fails else 0)

main()
