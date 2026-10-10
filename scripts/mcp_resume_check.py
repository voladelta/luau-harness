#!/usr/bin/env python3
"""Crash after every record of a run that calls MCP tools, resume, and check the server saw each call at most once
(exactly once when the crash was not inside the call). No API key needed."""
import json, os, shutil, subprocess, sys, tempfile, time
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent
HX = str(ROOT / "target/release/hx"); PORT = 8782
REPLIES = ['```luau\nprint(mcp.demo.add{a = 1, b = 2})\nprint(mcp.demo.delete_note{name = "x"})\nprint(mcp.demo.read_note{name = "y"})\n```',
           '```luau\nwrite("done.txt", "1")\n```', "All done."]

def run(ws, extra, env_extra=None):
    env = {**os.environ, "OPENROUTER_API_KEY": "x", **(env_extra or {})}
    return subprocess.run([HX, "run", "--workspace", str(ws), "--model", "m", "--api", "openai", "--base-url", f"http://127.0.0.1:{PORT}",
                           "--key-env", "OPENROUTER_API_KEY", "--max-turns", "6"] + extra, capture_output=True, text=True, env=env, timeout=120)

def newws(base):
    ws = Path(tempfile.mkdtemp(prefix="mcp-resume-", dir=base)).resolve(); (ws / ".harness").mkdir()
    (ws / "p.txt").write_text("call the tools\n")
    (ws / ".harness/mcp.toml").write_text(f'[server.demo]\ncommand = "python3"\nargs = ["{ROOT}/scripts/mcp_demo_server.py"]\nread_only_tools = ["read_*"]\n')
    (ws / ".harness/policy.toml").write_text(f'[fs]\nread = ["{ROOT}/scripts"]\n[approval]\nallow = ["mcp demo."]\n')
    return ws

def calls(ws):
    p = ws / "calls.log"; return p.read_text().split() if p.exists() else []

def lastlog(ws):
    return sorted((ws / ".harness/log").glob("*.jsonl"), key=lambda p: p.stat().st_mtime)[-1]

def main():
    env = {**os.environ, "MOCK_REPLIES": json.dumps(REPLIES), "MOCK_BY_LEN": "1"}
    mock = subprocess.Popen([sys.executable, str(ROOT / "scripts/mock_llm.py"), str(PORT)], env=env); time.sleep(1)
    base = tempfile.mkdtemp(prefix="hx-mcp-resume-"); fails = []; cases = 0
    try:
        b = newws(base); r = run(b, ["--prompt-file", str(b / "p.txt")]); assert r.returncode == 0, r.stderr
        assert sorted(calls(b)) == ["add", "delete_note", "read_note"] and (b / "done.txt").exists(), calls(b)
        recs = [json.loads(l) for l in lastlog(b).read_text().splitlines()]; n = len(recs)
        print(f"baseline ok: {n} records; kinds: {sorted({(r['kind'], r.get('tool')) for r in recs if r.get('tool','').startswith('mcp')})}")
        for k in range(1, n + 1):
            cases += 1; ws = newws(base)
            r1 = run(ws, ["--prompt-file", str(ws / "p.txt")], {"HX_TEST_CRASH_AFTER_RECORD": str(k)})
            log = lastlog(ws); r2 = run(ws, ["--prompt-file", str(ws / "p.txt"), "--resume", str(log)]) if r1.returncode != 0 else r1
            kind, tool = recs[k - 1]["kind"], recs[k - 1].get("tool", "")
            unknown = kind == "intent"
            c = calls(ws); problems = []
            if r2.returncode != 0: problems.append(f"resume failed: {r2.stderr[-200:]}")
            for name in ("add", "delete_note", "read_note"):
                if c.count(name) > 1: problems.append(f"REPEATED {name}: {c}")
            if not unknown and r2.returncode == 0 and (sorted(c) != ["add", "delete_note", "read_note"] or not (ws / "done.txt").exists()):
                problems.append(f"incomplete after resume: {c}")
            v = subprocess.run([HX, "log", "verify", str(log)], capture_output=True, text=True)
            if v.returncode != 0: problems.append(f"log: {v.stdout.strip()[:150]}")
            tag = f"crash after record {k} ({kind}:{tool})"
            if problems: fails.append(tag); print("FAIL", tag, problems)
            else: print("ok  ", tag, "(unknown outcome: never re-run)" if unknown else "")
    finally:
        mock.kill(); shutil.rmtree(base, ignore_errors=True)
    print(f"{cases - len(fails)} passed, {len(fails)} failed of {cases} crash points"); sys.exit(1 if fails else 0)
main()
