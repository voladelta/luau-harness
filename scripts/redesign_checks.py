#!/usr/bin/env python3
"""Checkpoint/supervisor acceptance checks. Local scripted providers only; no credentials or live API calls.

Artifacts are retained when HX_CHECK_ARTIFACTS is set. Run after cargo build --release.
"""
import contextlib
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[1]
HX = Path(os.environ.get("HX_BIN", ROOT / "target/release/hx")).resolve()
if os.environ.get("HX_CHECK_ARTIFACTS"):
    _artifact_root = Path(os.environ["HX_CHECK_ARTIFACTS"])
    _artifact_root.mkdir(parents=True, exist_ok=True)
    BASE = Path(tempfile.mkdtemp(prefix="redesign-", dir=_artifact_root))
else:
    BASE = Path(tempfile.mkdtemp(prefix="hx-redesign-"))
BASE.mkdir(parents=True, exist_ok=True)
ENV = {**os.environ, "HX_TRUST": "1", "HX_TEST_KEY": "local-mock-only"}
for key in ("HX_MODEL", "HX_API", "HX_BASE_URL", "HX_TEST_CRASH_AFTER_RECORD"):
    ENV.pop(key, None)
passed = 0


def workspace(name, init='require("std/recheck").enabled = false\n'):
    path = BASE / name
    path.mkdir(parents=True, exist_ok=True)
    (path / ".harness").mkdir(exist_ok=True)
    (path / ".harness/init.luau").write_text(init)
    (path / "task.md").write_text("Perform the scripted test task.")
    return path


def run(path, *args, env=None, ok=True):
    result = subprocess.run([str(HX), *args, "--workspace", str(path)], env={**ENV, **(env or {})},
                            capture_output=True, text=True, timeout=30)
    (path / "last.stdout").write_text(result.stdout)
    (path / "last.stderr").write_text(result.stderr)
    if ok:
        assert result.returncode == 0, (args, result.returncode, result.stdout, result.stderr)
    return result


def records(path):
    return [json.loads(line) for line in path.read_text().splitlines()]


def journal(path):
    files = list((path / ".harness/log").glob("*.jsonl"))
    assert len(files) == 1, files
    return files[0]


def tool(code):
    return {"code": code}


def body_text(body):
    return json.dumps(body)


@contextlib.contextmanager
def provider(reply):
    """A scripted endpoint."""
    requests = []
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with lock:
                index = len(requests)
                requests.append(body)
            answer = reply(body, index)
            if answer is None:
                self.connection.shutdown(socket.SHUT_RDWR)
                self.connection.close()
                return
            answer = {"text": answer} if isinstance(answer, str) else answer
            if "status" in answer:
                data = answer.get("error", "rejected").encode()
                self.send_response(answer["status"])
            else:
                calls = answer.get("calls", [tool(answer["code"])]) if "code" in answer else answer.get("calls", [])
                text = answer.get("text", "")
                usage = {"input_tokens": answer.get("tokens", 10), "output_tokens": 5}
                if self.path.endswith("responses"):
                    usage.update(answer.get("usage", {}))
                    output = [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}]
                    output += [{"type": "function_call", "call_id": f"c{index}_{i}", "name": c.get("name", "luau"),
                                "arguments": c.get("arguments", json.dumps({"code": c.get("code", "")}))} for i, c in enumerate(calls)]
                    data = json.dumps({"output": output, "usage": usage}).encode()
                else:
                    message = {"role": "assistant", "content": text or None}
                    if calls:
                        message["tool_calls"] = [{"id": f"c{index}_{i}", "type": "function", "function": {
                            "name": c.get("name", "luau"), "arguments": c.get("arguments", json.dumps({"code": c.get("code", "")}))}}
                            for i, c in enumerate(calls)]
                    usage = {"prompt_tokens": usage["input_tokens"], "completion_tokens": 5}
                    usage.update(answer.get("usage", {}))
                    data = json.dumps({"choices": [{"message": message, "finish_reason": "tool_calls" if calls else "stop"}], "usage": usage}).encode()
                self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            with contextlib.suppress(BrokenPipeError, ConnectionResetError):
                self.wfile.write(data)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.01), daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def start(path, url, *flags, env=None, ok=True):
    return run(path, "run", "--prompt-file", str(path / "task.md"), "--model", "mock",
               "--base-url", url, "--key-env", "HX_TEST_KEY", "--max-turns", "12", *flags, env=env, ok=ok)


def check(name, fn):
    global passed
    if os.environ.get("HX_CHECK_FILTER") and os.environ["HX_CHECK_FILTER"] not in name:
        return
    fn()
    passed += 1
    print(f"PASS {name}", flush=True)


def protocol(api):
    path = workspace("protocol-" + api)
    code = 'x=7; local t={n=3}; durable.set("saved",t); t.n=99; assert(durable.get("saved").n==3); assert(os.time()>0 and os.clock()>=0 and type(os.date())=="string"); assert(math.random(1,1)==1 and __nd_value==nil); print("live",x)'
    def answer(body, index):
        if index == 0:
            return tool(code)
        if index == 1:
            return tool('assert(x==7); assert(durable.get("saved").n==3); assert(_G==nil and coroutine==nil); print("checked")')
        return "done"
    with provider(answer) as (url, calls):
        report = json.loads(start(path, url, "--api", api).stdout)
        assert report["finished"] and report["luau_errors"] == 0 and len(calls) == 3, report
        log = journal(path)
        assert not any(r.get("tool") == "nd" or "tid" in r for r in records(log))
        restored = json.loads(run(path, "run", "--resume", str(log), env={"HX_MODEL": "wrong", "HX_BASE_URL": "http://invalid"}).stdout)
        assert restored["final_answer"] == "done" and len(calls) == 3
        result = run(path, "run", "--resume", str(log), "--model", "different", ok=False)
        assert result.returncode and "cannot override" in result.stderr


def crash_boundaries():
    code = 'x=7; write("once.txt","one"); print(sh("printf x >> append.txt").code); durable.set("saved",{n=1})'
    def answer(body, index):
        text = body_text(body)
        if "Recovered into a fresh Luau VM" in text:
            # A new recovery action, never the old code. Only inspect state and exercise copied saved data.
            if "RECOVERY_CHECKED" not in text:
                return tool('assert(x==nil); local d=durable.get("saved"); print("RECOVERY_CHECKED",d); print(pcall(read,"append.txt"))')
            return "recovered"
        return tool(code) if index == 0 else "done"
    with provider(answer) as (url, _):
        baseline = workspace("crash-baseline")
        assert json.loads(start(baseline, url).stdout)["luau_errors"] == 0
        baseline_records = records(journal(baseline))
    checked = 0
    for record in baseline_records:
        number = record["seq"]
        if number < 2:
            continue
        path = workspace(f"crash-{number:03d}")
        with provider(answer) as (url, calls):
            crashed = start(path, url, env={"HX_TEST_CRASH_AFTER_RECORD": str(number)}, ok=False)
            assert crashed.returncode != 0, (number, crashed.stdout, crashed.stderr)
            log = journal(path)
            # After manifest and before any dispatch, fresh recovery is also supported.
            resumed = run(path, "run", "--resume", str(log))
            report = json.loads(resumed.stdout)
            assert (report["finished"] or report["uncertain_operations"]) and report["luau_errors"] == 0, (number, report)
            assert not (path / "append.txt").exists() or (path / "append.txt").read_text() == "x", number
            old_chunks = [r for r in records(log) if r["kind"] == "chunk_begin" and r.get("code") == code]
            assert len(old_chunks) <= 1, number
            committed = [r for r in records(log) if r["kind"] == "checkpoint" and r.get("lane") == "root"]
            blob = (log.parent / "blobs" / committed[-1]["blob"]).read_bytes()
            assert hashlib.sha256(blob).hexdigest() == committed[-1]["blob"]
            count = len(calls)
            run(path, "run", "--resume", str(log))
            assert len(calls) == count, number
            checked += 1
    assert checked >= 10
    print(f"  {checked} crash boundaries, each resumed twice", flush=True)


def immutable_manifest():
    path = workspace("manifest")
    with provider(lambda body, i: tool('durable.set("keep",{v=9}); print("ok")') if i == 0 else "done") as (url, calls):
        start(path, url)
        log = journal(path)
        (path / ".harness/init.luau").write_text('error("new config must not execute")')
        (path / ".harness/policy.toml").write_text('[net]\nallow=true\n')
        report = json.loads(run(path, "run", "--resume", str(log)).stdout)
        assert report["finished"] and len(calls) == 2


def package_isolation():
    path = workspace("packages")
    directory = path / ".harness/pkgs/probe"
    directory.mkdir(parents=True)
    (directory / "hx.toml").write_text('[package]\nversion="1"\nkind="tool"\n[capabilities]\nfs=["read"]\n')
    (directory / "init.luau").write_text('''local n=0
return {
 data={10,20},
 count=function() n+=1; return n end,
 echo=function(t) t.n=8; return t end,
 write=function() return write("escape.txt","bad") end,
 import=function() return require("std/prompt") end,
 closure=function() return function() end end,
 read=function() return read("task.md") end,
}''')
    run(path, "approve", "probe")
    code = '''assert(tools.probe.count()==1 and tools.probe.count()==1)
assert(tools.probe.data[1]==10 and tools.probe.data[2]==20 and tools.probe.data["1"]==nil)
local t={n=1}; assert(tools.probe.echo(t).n==8 and t.n==1)
assert(not pcall(tools.probe.write)); assert(not pcall(tools.probe.import)); assert(not pcall(tools.probe.closure))
assert(not pcall(tools.probe.echo,function() end)); assert(tools.probe.read():find("scripted",1,true))
print("ISOLATION_OK")'''
    def answer(body, index):
        if index == 0:
            return tool(code)
        if index == 1:
            (directory / "init.luau").write_text('return {count=function() write("escape.txt","bad");return 99 end}')
            return tool('assert(tools.probe.count()==1); assert(refresh_extensions==nil); print("FROZEN_OK")')
        return "done"
    with provider(answer) as (url, _):
        report = json.loads(start(path, url).stdout)
        assert report["finished"] and report["luau_errors"] == 0, report
        assert not (path / "escape.txt").exists()


def batch_checks():
    path = workspace("batch")
    code = '''local t={{op="read",args={"task.md"}},{op="search",args={"scripted",{glob="*.md"}}}}
local r=batch(t); assert(r[1].ok and r[1].value:find("scripted",1,true)); assert(r[2].ok and #r[2].value>0)
assert(not pcall(batch,{{op="read",args={"task.md"}},{op="write",args={"escape","bad"}}}))
assert(not pcall(batch,{function() end})); assert(parallel==nil); print("BATCH_OK")'''
    with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, _):
        report = json.loads(start(path, url).stdout)
        assert report["luau_errors"] == 0, report
        reads = [r for r in records(journal(path)) if r["kind"] == "call" and r["tool"] == "read"]
        assert len(reads) == 1, reads


def body_messages(body):
    return body.get("messages") or body.get("input", [])


def has_assistant(body):
    return any(m.get("role") == "assistant" or m.get("type") == "function_call" for m in body_messages(body))


def task_text(body):
    messages = body_messages(body)
    user = next((m for m in messages if m.get("role") == "user"), None)
    if not user:
        return ""
    text = user.get("content", "")
    return text if isinstance(text, str) else "".join(p.get("text", "") for p in text)


def readonly_descendants_and_budget():
    path = workspace("readonly-children")
    directory = path / ".harness/pkgs/writer"
    directory.mkdir(parents=True)
    (directory / "hx.toml").write_text('[package]\nversion="1"\nkind="tool"\n[capabilities]\nfs=["write"]\n')
    (directory / "init.luau").write_text('return {run=function() return write("escape.txt","bad") end}')
    run(path, "approve", "writer")
    parent_code = '''local r=batch({{op="agent",args={"CHILD ONE"}},{op="agent",args={"CHILD TWO"}}})
for _, c in r do assert(c.ok and c.value.finished and c.value.luau_errors==0) end
print("CHILDREN_OK")'''
    child_code = '''assert(not pcall(write,"escape.txt","bad")); assert(not pcall(sh,"echo bad > escape.txt"))
assert(not pcall(tools.writer.run)); print("READONLY_OK")'''
    def answer(body, index):
        first = not has_assistant(body)
        if task_text(body).startswith("CHILD"):
            return tool(child_code) if first else "child done"
        return tool(parent_code) if first else "parent done"
    with provider(answer) as (url, calls):
        report = json.loads(start(path, url).stdout)
        assert report["finished"] and report["luau_errors"] == 0 and report["llm_calls"] == 6, report
        assert not (path / "escape.txt").exists()
        assert len(calls) == 6
    path = workspace("concurrent-budget")
    with provider(lambda body, i: tool(parent_code) if i == 0 else (time.sleep(0.15) or tool('print("child")'))) as (url, calls):
        report = json.loads(start(path, url, "--max-tokens", "30").stdout)
        assert not report["finished"] and "budget_exceeded" in report["stopped"], report
        assert len(calls) == 2 and report["llm_calls"] == 2 and report["budget"]["tokens"] == 30, report


def branch_saved_state():
    path = workspace("branch")
    with provider(lambda body, i: tool('x=9; durable.set("keep",{n=4}); print("saved")') if i == 0 else "done") as (url, _):
        start(path, url)
    parent = journal(path)
    with provider(lambda body, i: tool('assert(x==nil); assert(durable.get("keep").n==4); print("BRANCH_OK")') if i == 0 else "branched") as (url, calls):
        report = json.loads(run(path, "run", "--branch", str(parent), "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY").stdout)
        assert report["finished"] and report["luau_errors"] == 0 and len(calls) == 2, report
        assert Path(report["log"]) != parent
        assert any(r["kind"] == "branch" for r in records(Path(report["log"])))


def unknown_abandon():
    # --yolo runs sh without Seatbelt, so this check also runs where the sandbox is unavailable.
    path = workspace("abandon")
    code = 'print(sh("printf x >> again.txt").code)'
    with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, _):
        # Obtain the actual journal position of this operation from a control run.
        control = workspace("abandon-control")
        start(control, url, "--yolo")
        intent = next(r for r in records(journal(control)) if r["kind"] == "intent" and r["tool"] == "sh")
    with provider(lambda body, i: tool(code) if i == 0 or not (path / "again.txt").exists() else "done") as (url, _):
        start(path, url, "--yolo", env={"HX_TEST_CRASH_AFTER_RECORD": str(intent["seq"])}, ok=False)
        log = journal(path)
        original = next(r for r in records(log) if r["kind"] == "intent" and r["tool"] == "sh")
        assert f"UNKNOWN OUTCOME: #{original['seq']} sh" in run(path, "log", "verify", str(log), ok=False).stdout
        refused = run(path, "recover", str(log), str(original["seq"]), "--decision", "retry", "--yes", ok=False)
        assert refused.returncode and "complete or abandon" in refused.stderr
        run(path, "recover", str(log), str(original["seq"]), "--decision", "abandon", "--yes")
        again = run(path, "recover", str(log), str(original["seq"]), "--decision", "complete", "--yes", ok=False)
        assert again.returncode and "already resolved" in again.stderr
        report = json.loads(run(path, "run", "--resume", str(log)).stdout)
        assert report["finished"] and report["luau_errors"] == 0, report
        assert (path / "again.txt").read_text() == "x"


def local_write_reconciliation():
    code = 'write("note.txt","new"); print(edit("note.txt","new","newer"))'
    with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, _):
        control = workspace("reconcile-control", "")
        (control / "note.txt").write_text("old")
        start(control, url)
        assert (control / "note.txt").read_text() == "newer"
        log = records(journal(control))
    write_intent = next(r for r in log if r["kind"] == "intent" and r["tool"] == "write")
    expect = next(r for r in log if r["kind"] == "expect" and r["of"] == write_intent["seq"])
    edit_expect = next(r for r in log if r["kind"] == "expect" and r["of"] != write_intent["seq"])
    cases = [  # (crash after record, content to plant before resume, expected decision)
        (write_intent["seq"], None, "abandon"),       # before the file is touched
        (expect["seq"], None, "abandon"),             # expectation journaled, rename never happened
        (expect["seq"], "new", "complete"),           # rename landed, outcome record lost
        (edit_expect["seq"], "newer", "complete"),
    ]
    for number, plant, decision in cases:
        path = workspace(f"reconcile-{number}-{plant}", "")
        (path / "note.txt").write_text("old")
        with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, calls):
            assert start(path, url, env={"HX_TEST_CRASH_AFTER_RECORD": str(number)}, ok=False).returncode
            assert not list(path.glob(".note.txt.hx-tmp-*")) or plant is None
            if plant is not None:
                (path / "note.txt").write_text(plant)
            report = json.loads(run(path, "run", "--resume", str(journal(path))).stdout)
            assert report["finished"] and report["uncertain_operations"] == [], (number, report)
            resolutions = [r for r in records(journal(path)) if r["kind"] == "resolution"]
            assert [(r["decision"], r["by"]) for r in resolutions] == [(decision, "hx")], (number, resolutions)


def model_status_codes():
    # A rate limit is waited out (backing off 2 s, then 4 s) and releases each reservation: nothing was generated.
    path = workspace("status-429")
    with provider(lambda body, i: {"status": 429, "error": "slow down"} if i < 2 else "done") as (url, calls):
        report = json.loads(start(path, url, "--max-tokens", "1000000").stdout)
        assert report["finished"] and calls[0] == calls[2] and report["budget"]["reserved_tokens"] == 0, report
        failed = [r for r in records(journal(path)) if r["kind"] == "llm" and "error" in r]
        assert [r["billing_uncertain"] for r in failed] == [False, False], failed
    # A definite rejection releases the reservation, so the run is not left blocked behind it.
    path = workspace("status-400")
    with provider(lambda body, i: {"status": 400, "error": "bad request"} if i == 0 else "done") as (url, calls):
        start(path, url, "--max-tokens", "1000000", ok=False)
        assert len(calls) == 1
        report = json.loads(run(path, "run", "--resume", str(journal(path))).stdout)
        assert report["finished"] and report["budget"]["reserved_tokens"] == 0, report
        assert "UNKNOWN BILLING" not in run(path, "log", "verify", str(journal(path)), ok=False).stdout
    # A server error may follow a billed generation: one retry, and every failed attempt is charged its reserved tokens.
    path = workspace("status-500")
    with provider(lambda body, i: {"status": 502, "error": "upstream"}) as (url, calls):
        start(path, url, "--max-tokens", "1000000", ok=False)
        assert len(calls) == 2
        saved = records(journal(path))
        failed = [r for r in saved if r["kind"] == "llm"]
        assert [r["billing_uncertain"] for r in failed] == [True, True], failed
        assert [r["of"] for r in saved if r["kind"] == "billing"] == [r["of"] for r in failed]


def concurrent_children_under_token_cap():
    # Concurrent requests queue behind a running request's reservation instead of failing admission. Every request
    # reserves more than 200 tokens (its bytes plus the output cap), so under this cap one runs at a time.
    path = workspace("concurrent-cap")
    parent_code = '''local r=batch({{op="agent",args={"CHILD ONE"}},{op="agent",args={"CHILD TWO"}}})
for _, c in r do assert(c.ok and c.value.finished, c.error) end
print("CHILDREN_OK")'''
    def answer(body, index):
        first = not has_assistant(body)
        if task_text(body).startswith("CHILD"):
            time.sleep(0.15)
            return tool('print("child")') if first else "child done"
        return tool(parent_code) if first else "parent done"
    with provider(answer) as (url, calls):
        report = json.loads(start(path, url, "--max-tokens", "200").stdout)
        assert report["finished"] and report["luau_errors"] == 0 and len(calls) == 6, report
        assert report["budget"]["reserved_tokens"] == 0, report


def lost_response():
    # A lost response may have been billed: it is charged its reserved tokens. One new request follows; the lost
    # operation itself is never dispatched again.
    path = workspace("lost")
    with provider(lambda body, i: None) as (url, calls):
        result = start(path, url, ok=False)
        assert result.returncode and len(calls) == 2 and calls[0] == calls[1], (result.stderr, calls)
        log = records(journal(path))
        intents = [(r["seq"], r["reserved_tokens"]) for r in log if r["kind"] == "model_intent"]
        assert [(r["of"], r["tokens"]) for r in log if r["kind"] == "billing" and r["source"] == "hx"] == intents, log
        assert "UNKNOWN BILLING" not in run(path, "log", "verify", str(journal(path))).stdout
    # A crash after the lost response's record, before its charge, is charged on resume.
    path = workspace("lost-crash")
    with provider(lambda body, i: None) as (url, calls):
        control = workspace("lost-crash-control")
        start(control, url, ok=False)
        lost = next(r for r in records(journal(control)) if r["kind"] == "llm")
    with provider(lambda body, i: None if i == 0 else "done") as (url, calls):
        start(path, url, env={"HX_TEST_CRASH_AFTER_RECORD": str(lost["seq"])}, ok=False)
        assert "UNKNOWN BILLING" in run(path, "log", "verify", str(journal(path)), ok=False).stdout
        report = json.loads(run(path, "run", "--resume", str(journal(path))).stdout)
        assert report["finished"] and report["budget"]["reserved_tokens"] == 0, report
        assert [r["source"] for r in records(journal(path)) if r["kind"] == "billing"] == ["hx"]


def cached_token_budget():
    for api in ("completions", "responses"):
        path = workspace(f"cached-budget-{api}")
        prefix = "prompt" if api == "completions" else "input"
        usage = {f"{prefix}_tokens": 100, f"{prefix}_tokens_details": {"cached_tokens":90,"cache_write_tokens":9},
                 "completion_tokens" if api == "completions" else "output_tokens": 1}
        with provider(lambda body, i: {"code":"print(1)","usage":usage}) as (url, calls):
            report = json.loads(start(path, url, "--api", api, "--max-tokens", "100").stdout)
            assert len(calls) == 1 and report["budget"]["tokens"] == 101, report
            assert not report["finished"] and "token admission threshold" in report["stopped"], report


def children_run_concurrently():
    # Under a cap that fits both reservations, two children's requests are admitted side by side.
    path = workspace("children-concurrent")
    parent_code = '''local r=batch({{op="agent",args={"CHILD ONE"}},{op="agent",args={"CHILD TWO"}}})
for _, c in r do assert(c.ok and c.value.finished, c.error) end
print("CHILDREN_OK")'''
    active, peak = [0], [0]
    gate = threading.Lock()
    def answer(body, index):
        first = not has_assistant(body)
        if task_text(body).startswith("CHILD"):
            with gate:
                active[0] += 1
                peak[0] = max(peak[0], active[0])
            time.sleep(0.4)
            with gate:
                active[0] -= 1
            return tool('print("child")') if first else "child done"
        return tool(parent_code) if first else "parent done"
    with provider(answer) as (url, calls):
        report = json.loads(start(path, url, "--max-tokens", "1000000").stdout)
        assert report["finished"] and report["luau_errors"] == 0 and len(calls) == 6, report
        assert peak[0] == 2, peak


def sandboxed_sh_settles():
    # Sandboxed, no network: an interrupted sh is settled by hx as unknown_local and mutations continue on resume.
    # With the network on it could have acted elsewhere, so it waits for the owner. The crash precedes the command,
    # so this runs where Seatbelt is unavailable too.
    code = 'print(sh("printf x >> again.txt").code)'
    for name, flags, settled in [("offline", (), True), ("online", ("--net",), False)]:
        with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, _):
            control = workspace(f"sh-{name}-control")
            start(control, url, *flags, ok=False)
            intent = next(r for r in records(journal(control)) if r["kind"] == "intent" and r["tool"] == "sh")
        path = workspace(f"sh-{name}")
        answer = lambda body, i: tool(code) if i == 0 else tool('write("after.txt","ok")') if i == 1 else "done"
        with provider(answer) as (url, _):
            start(path, url, *flags, env={"HX_TEST_CRASH_AFTER_RECORD": str(intent["seq"])}, ok=False)
            report = json.loads(run(path, "run", "--resume", str(journal(path))).stdout)
            resolutions = [(r["decision"], r.get("by")) for r in records(journal(path)) if r["kind"] == "resolution"]
            if settled:
                assert resolutions[0] == ("unknown_local", "hx") and report["uncertain_operations"] == [], (resolutions, report)
                assert (path / "after.txt").read_text() == "ok", report
            else:
                assert not resolutions and report["uncertain_operations"] == [intent["seq"]], (resolutions, report)
                assert not (path / "after.txt").exists()


def compaction_and_recovery():
    pad = " -- " + "p" * 200
    sequence = [
        {"code": 'x=42; durable.set("x",42)' + pad},
        {"code": 'y=1' + pad, "tokens": 30000},
        "SUMMARY-ONE",
        {"code": 'assert(x==42); print("AFTER_ONE")' + pad},
        {"code": 'z=2' + pad, "tokens": 30000},
        "SUMMARY-TWO",
        {"code": 'assert(x==42); print("AFTER_TWO")' + pad},
        "done",
    ]
    init = 'require("std/recheck").enabled=false\nrequire("std/compact").keep_recent_tokens=40\n'
    path = workspace("compaction", init)
    with provider(lambda body, i: sequence[i]) as (url, calls):
        report = json.loads(start(path, url, "--context-window", "40000", "--max-output", "1000").stdout)
        assert report["finished"] and report["luau_errors"] == 0 and len(calls) == 8, report
        assert "<previous-summary>\\nSUMMARY-ONE" in body_text(calls[5])
        assert "SUMMARY-TWO" in body_text(calls[-1]) and "SUMMARY-ONE" not in body_text(calls[-1])
        log = journal(path)
        summary_seq = next(r["seq"] for r in records(log) if r["kind"] == "llm" and json.loads((log.parent / "blobs" / r["reply"]).read_text())["text"] == "SUMMARY-ONE")
    path = workspace("compaction-crash", init)
    def answer(body, index):
        text = body_text(body)
        if "Recovered into a fresh Luau VM" in text:
            return tool('assert(x==nil); assert(durable.get("x")==42); print("COMPACT_RECOVERED")') if "COMPACT_RECOVERED" not in text else "done"
        return sequence[index]
    with provider(answer) as (url, calls):
        start(path, url, "--context-window", "40000", "--max-output", "1000", env={"HX_TEST_CRASH_AFTER_RECORD": str(summary_seq)}, ok=False)
        assert not any("COMPACT_RECOVERED" in body_text(b) for b in calls)
        report = json.loads(run(path, "run", "--resume", str(journal(path))).stdout)
        assert report["finished"] and report["luau_errors"] == 0, report


def mcp_transport_uncertainty():
    tools = [{"name": "mutate", "description": "test mutation", "inputSchema": {"type": "object"}}]
    for transport, oversized in (("stdio", False), ("http", False), ("stdio", True), ("http", True)):
        path = workspace("mcp-" + transport + ("-oversized" if oversized else ""))
        server = None
        thread = None
        if transport == "stdio":
            script = path / "mcp-server.py"
            script.write_text('''import sys,json,os
for line in sys.stdin:
 m=json.loads(line)
 if "id" not in m: continue
 method=m["method"]
 if method=="initialize": r={"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1"}}
 elif method=="tools/list": r={"tools":[{"name":"mutate","description":"test mutation","inputSchema":{"type":"object"}}]}
 elif method=="tools/call":
  with open("mcp-calls.txt","a") as f: f.write("x")
  os._exit(0)
 else: r={}
 print(json.dumps({"jsonrpc":"2.0","id":m["id"],"result":r}),flush=True)
''')
            if oversized:
                script.write_text(script.read_text().replace('  os._exit(0)',
                    '  for _ in range(257): sys.stdout.write("x"*65536); sys.stdout.flush()\n  os._exit(0)'))
            config = f'[server.demo]\ncommand="python3"\nargs=[{json.dumps(str(script))}]\n'
        else:
            class McpHandler(BaseHTTPRequestHandler):
                def log_message(self, *args): pass
                def do_POST(self):
                    message = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    method = message["method"]
                    if method == "tools/call":
                        target = path / "mcp-calls.txt"
                        target.write_text((target.read_text() if target.exists() else "") + "x")
                        if oversized:
                            self.send_response(200)
                            self.send_header("Content-Type", "application/json")
                            self.send_header("Transfer-Encoding", "chunked")
                            self.end_headers()
                            with contextlib.suppress(BrokenPipeError, ConnectionResetError):
                                for _ in range(257):
                                    self.wfile.write(b"10000\r\n" + b"x" * 65536 + b"\r\n")
                                self.wfile.write(b"0\r\n\r\n")
                            return
                        self.connection.shutdown(socket.SHUT_RDWR)
                        self.connection.close()
                        return
                    if "id" not in message:
                        self.send_response(202); self.send_header("Content-Length", "0"); self.end_headers(); return
                    result = {"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"test","version":"1"}} if method == "initialize" else {"tools": tools}
                    data = json.dumps({"jsonrpc":"2.0","id":message["id"],"result":result}).encode()
                    self.send_response(200); self.send_header("Content-Type","application/json"); self.send_header("Content-Length",str(len(data))); self.end_headers(); self.wfile.write(data)
            server = ThreadingHTTPServer(("127.0.0.1", 0), McpHandler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            config = f'[server.demo]\nurl="http://127.0.0.1:{server.server_port}/mcp"\n'
        (path / ".harness/mcp.toml").write_text(config)
        (path / ".harness/policy.toml").write_text('[approval]\nallow=["mcp demo.mutate"]\n')
        code = '''local ok,err=pcall(mcp.demo.mutate,{})
assert(not ok and tostring(err):find("unknown_outcome",1,true))
assert(not pcall(write,"escape.txt","bad")); print("MCP_UNCERTAIN")'''
        try:
            with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, calls):
                report = json.loads(start(path, url).stdout)
                assert not report["finished"] and report["luau_errors"] == 0 and len(report["uncertain_operations"]) == 1, report
                assert (path / "mcp-calls.txt").read_text() == "x" and not (path / "escape.txt").exists()
                log = journal(path)
                count = len(calls)
                run(path, "run", "--resume", str(log))
                assert len(calls) == count and (path / "mcp-calls.txt").read_text() == "x"
                run(path, "recover", str(log), str(report["uncertain_operations"][0]), "--decision", "complete", "--yes")
                restored = json.loads(run(path, "run", "--resume", str(log)).stdout)
                assert restored["finished"] and len(calls) == count
        finally:
            if server:
                server.shutdown(); server.server_close(); thread.join()


def control_hook_error():
    path = workspace("hook-error", 'require("std/hooks").on("before_run",function() error("guard failed") end)')
    with provider(lambda body, i: tool('write("escape.txt","bad")')) as (url, _):
        result = start(path, url, ok=False)
        assert result.returncode and "control hook failed" in result.stderr
        assert not (path / "escape.txt").exists()


def review_boundary_regressions():
    for index, handler in enumerate([
        'function() return {deny="refused"} end',
        'function(p) p.action=nil; p.deny="refused" end',
    ]):
        path = workspace(f"finish-contract-{index}", f'require("std/hooks").on("before_finish",{handler})')
        with provider(lambda body, i: tool('print("checked")') if i == 0 else "done") as (url, calls):
            result = start(path, url, ok=False)
            assert result.returncode and "before_finish must return" in result.stderr and len(calls) == 2, result.stderr

    for index, command in enumerate([
        '{kind="finish",report="oops"}',
        '{kind="model",request={system=8,messages="oops"}}',
        '{kind="run",code={}}',
    ]):
        path = workspace(f"command-contract-{index}")
        override = path / ".harness/override/std"
        override.mkdir(parents=True)
        (override / "loop.luau").write_text('return {init=function() return {} end, step=function(s,e) return {phase="finished"},' + command + ' end}')
        with provider(lambda body, i: "unused") as (url, calls):
            result = start(path, url, ok=False)
            assert result.returncode not in (0, 101) and not calls, result.stderr
            assert not any(r["kind"] == "checkpoint" for r in records(journal(path)))

    path = workspace("bounded-durable")
    code = '''local t={x=1}; for i=1,30 do t={a=t,b=t} end
assert(not pcall(durable.set,"dag",t)); assert(durable.get("dag")==nil)
local s=string.rep("x",1024*1024)
for i=1,15 do durable.set(tostring(i),s) end
assert(not pcall(durable.set,"16",s)); assert(durable.get("16")==nil)
assert(#durable.get("1")==#s); print("BOUNDED_OK")'''
    with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, calls):
        report = json.loads(start(path, url).stdout)
        assert report["finished"] and report["luau_errors"] == 0, report

    path = workspace("package-native-arrays")
    directory = path / ".harness/pkgs/child"
    directory.mkdir(parents=True)
    (directory / "hx.toml").write_text('[package]\nversion="1"\nkind="tool"\n[capabilities]\nllm=["agent"]\n')
    (directory / "init.luau").write_text('return {run=function() return agent("PACKAGE CHILD",{model="mock",effort="medium",max_turns=2}) end}')
    run(path, "approve", "child")
    def answer(body, index):
        if task_text(body).startswith("PACKAGE CHILD"):
            return "child done" if has_assistant(body) else tool('print("CHILD")')
        return tool('local r=tools.child.run(); durable.set("report",r); assert(r.finished); print("ARRAY_OK")') if index == 0 else "done"
    with provider(answer) as (url, calls):
        report = json.loads(start(path, url).stdout)
        assert report["finished"] and report["luau_errors"] == 0 and len(calls) == 4, report
        intent = next(r for r in records(journal(path)) if r["kind"] == "intent" and r["tool"] == "agent")
        assert intent["args"]["model"] == "mock" and intent["args"]["effort"] == "medium", intent
        log = journal(path)
        checkpoint = next(r for r in reversed(records(log)) if r["kind"] == "checkpoint" and r["lane"] == "root")
        saved = json.loads((log.parent / "blobs" / checkpoint["blob"]).read_text())
        assert saved["durable"]["report"]["uncertain_operations"] == [], saved["durable"]


def review_branch_and_revocation():
    path = workspace("branch-start-crash")
    with provider(lambda body, i: tool('durable.set("keep",{n=4})') if i == 0 else "done") as (url, calls):
        start(path, url)
    parent = journal(path)
    for boundary in (2, 3):
        with provider(lambda body, i: tool('assert(durable.get("keep").n==4); print("SEED_OK")') if i == 0 else "done") as (url, calls):
            previous = set((path / ".harness/log").glob("*.jsonl"))
            result = run(path, "run", "--branch", str(parent), "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY",
                         env={"HX_TEST_CRASH_AFTER_RECORD": str(boundary)}, ok=False)
            assert result.returncode and not calls
            child, = set((path / ".harness/log").glob("*.jsonl")) - previous
            report = json.loads(run(path, "run", "--resume", str(child)).stdout)
            assert report["finished"] and report["luau_errors"] == 0, report

    # Networked sh interrupted by a crash could have acted elsewhere, so only the owner can settle it.
    code = 'print(sh("printf x >> again.txt").code)'
    with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, _):
        control = workspace("revoked-owner-reconciliation-control")
        start(control, url, "--net", ok=False)
        op = next(r for r in records(journal(control)) if r["kind"] == "intent" and r["tool"] == "sh")["seq"]
    path = workspace("revoked-owner-reconciliation")
    with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, calls):
        assert start(path, url, "--net", env={"HX_TEST_CRASH_AFTER_RECORD": str(op)}, ok=False).returncode
        log = journal(path)
        count = len(calls)
        result = run(path, "run", "--branch", str(log), "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY", ok=False)
        assert result.returncode and "reconcile uncertain" in result.stderr
        assert sum(any(r["kind"] == "manifest" for r in records(p)) for p in (path / ".harness/log").glob("*.jsonl")) == 1
        run(path, "revoke", str(log))
        run(path, "recover", str(log), str(op), "--decision", "abandon", "--yes")
        assert run(path, "run", "--resume", str(log), ok=False).returncode and len(calls) == count
    with provider(lambda body, i: "branched") as (url, calls):
        report = json.loads(run(path, "run", "--branch", str(log), "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY").stdout)
        assert report["finished"], report


def controller_no_authority():
    path = workspace("controller-authority", 'write("escape.txt","bad")')
    with provider(lambda body, i: "unused") as (url, calls):
        result = start(path, url, ok=False)
        assert result.returncode and not calls and not (path / "escape.txt").exists()


def single_writer_and_revocation():
    path = workspace("lock")
    entered, release = threading.Event(), threading.Event()
    def answer(body, i):
        entered.set()
        assert release.wait(15)
        return tool('write("escape.txt","bad")')
    with provider(answer) as (url, calls):
        process = subprocess.Popen([str(HX), "run", "--workspace", str(path), "--prompt-file", str(path / "task.md"),
                                    "--model", "mock", "--base-url", url, "--key-env", "HX_TEST_KEY"], env=ENV,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            assert entered.wait(10)
            log = journal(path)
            before = log.read_bytes()
            refused = run(path, "run", "--resume", str(log), ok=False)
            assert refused.returncode and "already open" in refused.stderr
            assert log.read_bytes() == before
            run(path, "revoke", str(log))
            release.set()
            stdout, stderr = process.communicate(timeout=15)
            assert process.returncode and "revoked" in stderr, (stdout, stderr)
            assert not (path / "escape.txt").exists() and len(calls) == 1
        finally:
            release.set()
            if process.poll() is None:
                process.kill()
                process.wait()


def deadline():
    path = workspace("deadline")
    with provider(lambda body, i: tool('print(sh("sleep 2; echo bad > late.txt",{timeout=10}).code)')) as (url, calls):
        started = time.monotonic()
        result = start(path, url, "--max-wall", "0.6", ok=False)
        assert time.monotonic() - started < 2
        report = json.loads(result.stdout)
        assert not report["finished"] and "turns" not in report, report
        time.sleep(2.1)
        assert not (path / "late.txt").exists()
        count = len(calls)
        for _ in range(2):
            resumed = json.loads(run(path, "run", "--resume", str(journal(path))).stdout)
            assert not resumed["finished"] and "deadline" in resumed["stopped"] and len(calls) == count, resumed


def completed_deadline_resume():
    for custom in (False, True):
        path = workspace(f"completed-deadline-{custom}")
        if custom:
            override = path / ".harness/override/std"
            override.mkdir(parents=True)
            # Neither phase nor report belongs to this controller's state. Resume must use the host's finish receipt.
            (override / "loop.luau").write_text('''return {
init=function() return {private_counter=0} end,
step=function(s,e)
    assert(e.kind=="next", "finished controller must not run on recovery")
    return {private_counter=1}, {kind="finish", report={finished=true,final_answer="custom done",turns=0}}
end}''')
        with provider(lambda body, i: tool('print("checked")') if i == 0 else "done") as (url, calls):
            first = json.loads(start(path, url, "--max-wall", "2").stdout)
            assert first["finished"], first
            log = journal(path)
            saved = records(log)
            last = next(r for r in reversed(saved) if r["kind"] == "checkpoint")
            checkpoint = json.loads((log.parent / "blobs" / last["blob"]).read_text())
            assert checkpoint["status"] == "finished" and "controller" not in checkpoint, checkpoint
            manifest = next(r for r in saved if r["kind"] == "manifest")
            deadline = json.loads((log.parent / "blobs" / manifest["blob"]).read_text())["deadline"]
            time.sleep(max(0, deadline - time.time()) + 0.1)
            count = len(calls)
            for _ in range(2):
                restored = json.loads(run(path, "run", "--resume", str(log)).stdout)
                assert restored["finished"] and restored["final_answer"] == first["final_answer"], restored
                assert len(calls) == count


def controller_deadlines():
    for name, source in [
        ("next", 'return {init=function() return {} end, step=function(s,e) return s,{kind="next"} end}'),
        ("next_call", 'return {init=function() return {} end, step=function(s,e) return s,{kind="next_call"} end}'),
        ("step", 'return {init=function() return {} end, step=function() print("IN_CONTROLLER"); while true do end end}'),
        ("init", 'return {init=function() print("IN_CONTROLLER"); while true do end end}'),
        ("load", 'print("IN_CONTROLLER"); while true do end'),
    ]:
        path = workspace(f"controller-deadline-{name}")
        override = path / ".harness/override/std"
        override.mkdir(parents=True)
        (override / "loop.luau").write_text(source)
        with provider(lambda body, i: "unused") as (url, calls):
            started = time.monotonic()
            result = start(path, url, "--max-wall", "0.3")
            assert time.monotonic() - started < 2
            report = json.loads(result.stdout)
            assert not report["finished"] and "absolute run deadline" in report["stopped"] and not calls, report
            if name in ("step", "init", "load"):
                assert "IN_CONTROLLER" in result.stderr, result.stderr
            restored = json.loads(run(path, "run", "--resume", str(journal(path))).stdout)
            assert restored["stopped"] == report["stopped"] and not calls, restored


def cli_preflight():
    for index, flags in enumerate([
        ["--max-wall"], ["--max-wall", "--net"], ["--workspace"], ["--model"],
        ["--max-wall", "NaN"], ["--max-wall", "-1"], ["--max-output", "0"], ["--max-cost", "1"],
        ["--max-toknes", "10"], ["--net", "--no-net"],
        ["--max-wall", "1", "--max-wall", "2"], ["--resume", "absent.jsonl", "--model", "mock"],
        ["--api", "typo"],
    ]):
        path = workspace(f"cli-preflight-{index}")
        # Keep the invalid flag last when appropriate: run() appends --workspace and would mask that regression.
        result = subprocess.run([str(HX), "run", "--workspace", str(path), *flags], env=ENV,
                                capture_output=True, text=True, timeout=10)
        assert result.returncode == 2, (flags, result.returncode, result.stderr)
        assert not (path / ".harness/log").exists(), (flags, "invalid invocation opened a journal")
    assert not Path("absent.lock").exists()


def unsupported_run_format():
    path = workspace("unsupported-format")
    with provider(lambda body, i: "done") as (url, calls):
        start(path, url)
        log = journal(path)
        saved = records(log)
        manifest = next(r for r in saved if r["kind"] == "manifest")
        content = json.loads((log.parent / "blobs" / manifest["blob"]).read_text())
        content["format"] = manifest["format"] = 0
        data = json.dumps(content).encode()
        manifest["blob"] = hashlib.sha256(data).hexdigest()
        (log.parent / "blobs" / manifest["blob"]).write_bytes(data)
        previous, lines = "genesis", []
        for record in saved:
            record["prev"] = previous
            line = json.dumps(record, separators=(",", ":"))
            lines.append(line)
            previous = hashlib.sha256(line.encode()).hexdigest()
        log.write_text("\n".join(lines) + "\n")
        count = len(calls)
        for flag in ("--resume", "--branch"):
            refused = run(path, "run", flag, str(log), ok=False)
            assert refused.returncode and "unsupported run format 0" in refused.stderr, refused.stderr
            assert len(calls) == count



def agent_option_validation():
    path = workspace("agent-options")
    package = path / ".harness/pkgs/child"
    package.mkdir(parents=True)
    (package / "hx.toml").write_text('[package]\nversion="1"\nkind="tool"\n[capabilities]\nllm=["agent"]\n')
    (package / "init.luau").write_text('return {run=function(opts) return agent("child",opts) end}')
    run(path, "approve", "child")
    code = '''for _, opts in {{effort="turbo"}, {model=7}, {max_turns="many"}, {max_turns=-1}} do
    assert(not pcall(agent,"child",opts))
    assert(not pcall(tools.child.run,opts))
    assert(not pcall(batch,{{op="read",args={"task.md"}},{op="agent",args={"child",opts}}}))
end
print("OPTIONS_VALIDATED")'''
    with provider(lambda body, i: tool(code) if i == 0 else "done") as (url, calls):
        report = json.loads(start(path, url).stdout)
        assert report["finished"] and report["luau_errors"] == 0 and len(calls) == 2, report
        assert not any(r.get("tool") in ("read", "agent") for r in records(journal(path)))


def corrupt_artifacts():
    path = workspace("corrupt")
    with provider(lambda body, i: tool('print(1)') if i == 0 else "done") as (url, calls):
        start(path, url)
        log = journal(path)
        manifest = next(r for r in records(log) if r["kind"] == "manifest")
        blob = log.parent / "blobs" / manifest["blob"]
        blob.write_bytes(blob.read_bytes() + b" ")
        count = len(calls)
        result = run(path, "run", "--resume", str(log), ok=False)
        assert result.returncode and "hash mismatch" in result.stderr and len(calls) == count


if __name__ == "__main__":
    check("chat protocol, live globals, copied durable state, completed resume", lambda: protocol("completions"))
    check("responses protocol and frozen launch settings", lambda: protocol("responses"))
    check("crash recovery never reruns old chunks", crash_boundaries)
    check("workspace edits do not alter a frozen manifest", immutable_manifest)
    check("isolated stateless packages and transitive capability boundaries", package_isolation)
    check("sealed batches reject mutations and preserve result order", batch_checks)
    check("read-only descendants cannot regain package authority; concurrent reservations", readonly_descendants_and_budget)
    check("explicit branches inherit only committed data and select a new manifest", branch_saved_state)
    check("owner abandon unblocks a resumed run; retry is no longer a decision", unknown_abandon)
    check("crash-interrupted write/edit settle from the files without the owner", local_write_reconciliation)
    check("rate limits are waited out; definite rejections release reservations", model_status_codes)
    check("concurrent children queue under a token cap instead of failing", concurrent_children_under_token_cap)
    check("lost responses are charged their reserved tokens; one new request follows", lost_response)
    check("cached reads and writes count toward the total token budget", cached_token_budget)
    check("children's requests run side by side under a cap that fits both", children_run_concurrently)
    check("sandboxed offline sh settles itself; networked sh waits for the owner", sandboxed_sh_settles)
    check("compaction replaces previous summaries and checkpoints explicit state", compaction_and_recovery)
    check("MCP lost and oversized mutation responses stay uncertain without redispatch", mcp_transport_uncertainty)
    check("control hook failure prevents dispatch", control_hook_error)
    check("review regressions: hook/command validation, bounded copies, native arrays", review_boundary_regressions)
    check("review regressions: branch startup crashes and revoked reconciliation", review_branch_and_revocation)
    check("controller has no ambient effect authority", controller_no_authority)
    check("single writer and owner revocation", single_writer_and_revocation)
    check("absolute deadline cancels local work and includes downtime", deadline)
    check("completed runs retain their answer after deadlines, including custom controllers", completed_deadline_resume)
    check("controller-only transitions and CPU loops obey the absolute deadline", controller_deadlines)
    check("invalid CLI options fail before opening a journal", cli_preflight)
    check("unsupported run formats fail before resume or branch dispatch", unsupported_run_format)
    check("session, package and batch agents reject invalid options before dispatch", agent_option_validation)
    check("corrupt artifacts fail before dispatch", corrupt_artifacts)
    print(f"{passed} passed; artifacts: {BASE}")
