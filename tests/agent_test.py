#!/usr/bin/env python3
"""End-to-end checks of hx against a scripted Anthropic Messages server.

usage: agent_test.py PATH_TO_HX [unittest arguments]

Each test gets a fresh project directory and a fresh fake home, and scripts the model's replies. The sandbox
checks need a kernel with Landlock.
"""

import http.server
import json
import os
import pty
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HX = None
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
KEY = "sk-test-0123456789"


class Model(http.server.ThreadingHTTPServer):
    """Answers each request with the next scripted reply and records what was asked."""

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.script = []
        self.requests = []
        self.headers = []
        self.hanging = threading.Event()
        threading.Thread(target=self.serve_forever, daemon=True).start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_address[1]}/v1/messages"


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        self.server.requests.append(json.loads(body))
        self.server.headers.append({name.lower(): value for name, value in self.headers.items()})
        status, reply = self.server.script.pop(0) if self.server.script else (500, {"error": "script ran out"})
        if status == "hang":  # hold the request open until the client gives up
            self.server.hanging.set()
            time.sleep(reply)
            return
        data = json.dumps(reply).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


class Tools(http.server.ThreadingHTTPServer):
    """A stand-in for DeepSeek's tool endpoints: answers by path and records each request, body as bytes."""

    def __init__(self):
        super().__init__(("127.0.0.1", 0), ToolsHandler)
        self.requests = []
        threading.Thread(target=self.serve_forever, daemon=True).start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_address[1]}/v1/tools"


class ToolsHandler(http.server.BaseHTTPRequestHandler):
    ANSWERS = {
        "/v1/tools/search": {"results": [{"rerank_score": 2.1, "title": "Luau", "url": "https://luau.org",
                                          "snippet": "s", "summary": "A small fast language.", "content": "",
                                          "site_name": None, "site_icon": None, "date": None, "authority": 3}]},
        "/v1/tools/fetch": {"title": "Luau", "content": "# Luau\n\nA small fast language."},
        "/exa/search": {"requestId": "r1", "costDollars": {"total": 0.012},
                        "results": [{"title": "Luau", "url": "https://luau.org", "id": "x",
                                     "publishedDate": "2026-08-26T00:00:00.000Z", "author": None,
                                     "highlights": ["Luau is a small fast language.", "It has gradual types."]}],
                        "output": {"content": "Luau is a fast, small, gradually typed language.",
                                   "grounding": [{"field": "content", "confidence": "high",
                                                  "citations": [{"url": "https://luau.org", "title": "Luau"}]}]},
                        "notices": [{"code": "RESTRICTIONS_RELAXED", "path": "/query", "message": "site: removed."}],
                        "warnings": [{"code": "IGNORED_FIELD", "path": "/x", "message": "x is ignored."}]},
        "/v1/tools/ocr": {"id": "ocr-1", "object": "ocr.result", "created": 0, "purpose": "x", "page_count": 1,
                          "pages": [{"index": 0, "text": "Recognized words", "finish_reason": "stop"}],
                          "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}},
    }

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        self.server.requests.append({"path": self.path, "body": body,
                                     "headers": {k.lower(): v for k, v in self.headers.items()}})
        answer = self.ANSWERS.get(self.path)
        data = (b"\n\n" if self.path.endswith("/ocr") else b"") + json.dumps(answer or {"error": "no such tool"}).encode()
        self.send_response(200 if answer else 404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


def read(*path):
    with open(os.path.join(*path)) as f:
        return f.read()


def kill_quietly(pid):
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def reply(content,stop="end_turn", input_tokens=10):
    usage = {"input_tokens": input_tokens, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "output_tokens": 5}
    return 200, {"id": "msg_1", "type": "message", "role": "assistant", "content": content, "stop_reason": stop,
                 "stop_sequence": None, "usage": usage}


def text(words):
    return {"type": "text", "text": words}


# The model has one tool, lua, and calls the functions from its code. A scripted bash, read, write or edit call
# becomes the code a model would send for it: `return bash("...")`.
FUNCTIONS = {"bash": ["command", "timeout"], "read": ["path", "offset", "limit"], "write": ["path", "content"],
             "edit": ["path", "old", "new", "all"], "submit": ["note"]}


def lua_literal(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    out = []
    for c in value:
        if c in '"\\':
            out.append("\\" + c)
        elif c == "\n":
            out.append("\\n")
        elif c.isprintable() or c == "\t":
            out.append(c)
        else:
            out.extend(f"\\{b:03d}" for b in c.encode())
    return '"' + "".join(out) + '"'


def code(name, args):
    values = [args.get(key) for key in FUNCTIONS[name]]
    while values and values[-1] is None:
        values.pop()
    return f"return {name}({', '.join('nil' if v is None else lua_literal(v) for v in values)})"


def use(name, args, call_id="call_1"):
    if name in FUNCTIONS:
        name, args = "lua", {"code": code(name, args)}
    return {"type": "tool_use", "id": call_id, "name": name, "input": args}


def say(words, **kw):
    return reply([text(words)], **kw)


def call(name, args, call_id="call_1", stop="tool_use", **kw):
    return reply([use(name, args, call_id)], stop=stop, **kw)


class AgentTest(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="hx-test-")
        self.project = os.path.join(self.root, "project")
        self.home = os.path.join(self.root, "home")
        os.makedirs(self.project)
        os.makedirs(self.home)
        self.model = Model()

    def tearDown(self):
        self.model.shutdown()
        self.model.server_close()
        shutil.rmtree(self.root, ignore_errors=True)

    def env(self, **extra):
        # Without the caller's own hx settings, or a test's result would depend on the shell it was run from.
        env = {k: v for k, v in os.environ.items()
               if not k.lower().endswith("_proxy") and not k.startswith("HX_") and k != "ANTHROPIC_API_KEY"}
        env.update(HOME=self.home, HX_API_URL=self.model.url, HX_KEY_ENV="HX_TEST_KEY", HX_TEST_KEY=KEY,
                   NO_PROXY="127.0.0.1", DEEPSEEK_API_KEY="", EXA_API_KEY="")
        env.update(extra)
        return env

    def run_hx(self, *args, script=(), stdin=None, **env):
        self.model.script = list(script)
        return subprocess.run([HX, *args], cwd=self.project, env=self.env(**env), input=stdin,
                              capture_output=True, text=True, timeout=60)

    def tool_results(self):
        """The content of every tool result the model was sent, in order, without repeats."""
        seen, out = set(), []
        for request in self.model.requests:
            for message in request["messages"]:
                if message["role"] != "user" or isinstance(message["content"], str):
                    continue
                for block in message["content"]:
                    if block["type"] == "tool_result" and block["tool_use_id"] not in seen:
                        seen.add(block["tool_use_id"])
                        out.append(block["content"])
        return out

    # --- the loop ---

    def test_a_tool_call_runs_and_its_result_goes_back(self):
        done = self.run_hx("make a file", script=[call("bash", {"command": "echo hi > out.txt; echo made"}), say("All done.")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "All done.\n")
        self.assertEqual(read(self.project, "out.txt"), "hi\n")
        first, second = self.model.requests
        self.assertIn("coding agent", first["system"])
        self.assertIn("The kernel confines you", first["system"])
        self.assertEqual(first["messages"], [{"role": "user", "content": "make a file"}])
        self.assertEqual([t["name"] for t in first["tools"]], ["lua"])
        self.assertEqual(set(first["tools"][0]), {"name", "description", "input_schema"})
        self.assertEqual(first["thinking"], {"type": "adaptive"})
        self.assertEqual(second["messages"][1:], [
            {"role": "assistant", "content": [use("bash", {"command": "echo hi > out.txt; echo made"})]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "made\n[exit 0]"}]},
        ])
        # Everything before the new messages is resent unchanged, which is what prompt caching needs.
        self.assertEqual(second["messages"][0], first["messages"][0])
        self.assertEqual((second["system"], second["tools"]), (first["system"], first["tools"]))

    def test_thinking_blocks_go_back_exactly_as_they_came(self):
        blocks = [{"type": "thinking", "thinking": "", "signature": "sig-1"}, text("Looking."),
                  {"type": "thinking", "thinking": "run ls\né", "signature": "sig-2"}, use("bash", {"command": "true"})]
        done = self.run_hx("go", script=[reply(blocks, stop="tool_use"), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[1]["messages"][1], {"role": "assistant", "content": blocks})
        self.assertEqual(done.stdout, "Looking.\nok\n")  # thinking is not printed

    def test_several_calls_in_one_reply_are_answered_in_one_message(self):
        done = self.run_hx("go", script=[reply([use("bash", {"command": f"echo {i}"}, f"c{i}") for i in range(3)], stop="tool_use"),
                                         say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual([m["role"] for m in self.model.requests[1]["messages"]], ["user", "assistant", "user"])
        self.assertEqual(self.tool_results(), ["0\n[exit 0]", "1\n[exit 0]", "2\n[exit 0]"])

    def test_a_bad_tool_call_is_reported_to_the_model_not_fatal(self):
        done = self.run_hx("go", script=[call("lua", "not an object", "c1"),
                                         reply([{"type": "tool_use", "id": "c2", "name": "bash", "input": {"command": "true"}}],
                                               stop="tool_use"),
                                         call("nope", {}, "c3"), call("lua", {}, "c4"),
                                         call("lua", {"code": "return read({file_path = 'f.txt'})"}, "c5"),
                                         call("lua", {"code": "return +"}, "c6"), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        results = self.tool_results()
        self.assertIn("not an object", results[0])
        # A model used to the old tools may still call one by name.
        self.assertEqual(results[1], "error: there is no tool named bash")
        self.assertEqual(results[2], "error: there is no tool named nope")
        self.assertEqual(results[3], 'error: missing argument "code": lua takes code')
        self.assertEqual(results[4], "error: read: argument 1 (path) must be a string, not table; usage: read(path, offset?, limit?)")
        self.assertIn("error: lua:1:", results[5])
        failed = self.model.requests[1]["messages"][-1]["content"][0]
        self.assertIs(failed["is_error"], True)

    def test_an_empty_reply_is_asked_for_again(self):
        done = self.run_hx("go", script=[reply([]), reply([text("")]), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "ok\n")
        self.assertEqual(len(self.model.requests), 3)
        self.assertEqual(self.model.requests[0], self.model.requests[2])

    def test_an_empty_refusal_is_an_answer(self):
        done = self.run_hx("go", script=[reply([], stop="refusal")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(self.model.requests), 1)
        self.assertIn("declined", done.stderr)

    def test_a_reply_of_thinking_alone_is_not_empty(self):
        done = self.run_hx("go", script=[reply([{"type": "thinking", "thinking": "hm", "signature": "s"}])])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(self.model.requests), 1)

    def reminders(self):
        """The text the loop added behind the tool results of each request after the first."""
        return [[b["text"] for b in r["messages"][-1]["content"] if b["type"] == "text"] for r in self.model.requests[1:]]

    def test_a_repeated_call_gets_a_reminder(self):
        same = [call("bash", {"command": "echo same", "timeout": 5}, f"c{i}") for i in range(5)]
        done = self.run_hx("go", script=[*same, say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        reminders = self.reminders()
        self.assertEqual([len(r) for r in reminders], [0, 0, 1, 0, 1])
        self.assertIn('bash("echo same", 5) has now returned the same result 3 times', reminders[2][0])
        self.assertIn('bash("echo same", 5) has returned the same result 5 times in this task', reminders[4][0])

    def test_a_silent_command_that_succeeds_is_not_counted(self):
        # git add, mkdir or a sleep between steps does something; doing it again is not a sign of being stuck.
        same = [call("bash", {"command": "true"}, f"c{i}") for i in range(5)]
        done = self.run_hx("go", script=[*same, say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual([len(r) for r in self.reminders()], [0, 0, 0, 0, 0])

    def test_a_loop_inside_one_call_is_counted_too(self):
        # The model's code prints a counter, so no two tool results are alike; the function results are.
        loop = "for i = 1, 6 do print(i, bash('echo not ready')) end"
        done = self.run_hx("go", script=[call("lua", {"code": loop}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        reminders = self.reminders()
        self.assertEqual([len(r) for r in reminders], [1])  # one reminder, for the highest count passed: 5
        self.assertIn('bash("echo not ready") has returned the same result 6 times', reminders[0][0])

    def test_a_call_polled_between_other_calls_still_counts(self):
        poll = lambda i: call("bash", {"command": "echo not ready"}, f"p{i}")
        wait = lambda i: call("bash", {"command": f"sleep 0.{i}"}, f"w{i}")
        done = self.run_hx("go", script=[poll(1), wait(1), poll(2), wait(2), poll(3), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual([len(r) for r in self.reminders()], [0, 0, 0, 0, 1])

    def test_a_call_whose_result_changes_does_not_count(self):
        # A test run again after each fix: the same call, a different result each time.
        runs = [call("bash", {"command": "echo x >> log; wc -l < log"}, f"c{i}") for i in range(4)]
        done = self.run_hx("go", script=[*runs, say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual([len(r) for r in self.reminders()], [0, 0, 0, 0])

    def test_a_long_result_is_counted_by_all_of_it(self):
        # Long results are counted by a digest, not kept: one byte changed in the middle is still a new result.
        loop = ('local a = string.rep("a", 300000) '
                'for i = 1, 4 do write("big", a:sub(1, 150000) .. i .. a:sub(150002)) bash("cat big") end '
                'for i = 1, 3 do bash("cat big") end')
        done = self.run_hx("go", script=[call("lua", {"code": loop}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        reminders = self.reminders()
        self.assertEqual([len(r) for r in reminders], [1])
        self.assertIn('bash("cat big") has now returned the same result 4 times', reminders[0][0])

    def test_loading_an_extension_again_is_not_counted(self):
        # ext returns the same table of functions each time, which says nothing about progress.
        done = self.run_hx("go", script=[call("lua", {"code": "for i = 1, 6 do ext('web') end return 'ok'"}),
                                         say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results(), ["ok"])
        self.assertEqual([len(r) for r in self.reminders()], [0])

    def test_the_log_is_saved_during_the_turn(self):
        # A run killed half-way leaves its log up to the last tool call, not nothing.
        os.makedirs(os.path.join(self.project, ".hx"))
        done = self.run_hx("go", script=[call("bash", {"command": "echo first"}, "c1"),
                                         call("bash", {"command": "cat .hx/log/*.json"}, "c2"), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        during = json.loads(self.tool_results()[1].rsplit("\n", 1)[0])
        self.assertEqual([m["role"] for m in during["messages"]], ["user", "assistant", "user"])
        self.assertEqual(during["messages"][2]["content"][0]["content"], "first\n[exit 0]")
        logs = [name for name in os.listdir(os.path.join(self.project, ".hx", "log")) if name.endswith(".json")]
        self.assertEqual(len(logs), 1)
        self.assertEqual(len(json.loads(read(self.project, ".hx", "log", logs[0]))["messages"]), 6)

    def test_sessions_in_the_same_second_keep_their_own_logs(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        for _ in range(3):
            self.assertEqual(self.run_hx("go", script=[say("ok")]).returncode, 0)
        logs = [name for name in os.listdir(os.path.join(self.project, ".hx", "log")) if name.endswith(".json")]
        self.assertEqual(len(logs), 3)

    def test_running_out_of_replies_still_gets_a_report(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        with open(os.path.join(self.project, ".hx", "loop.luau"), "w") as f:
            f.write("local M = assert(host.load(host.default_loop, '=loop'))() M.max_replies = 2 return M")
        done = self.run_hx("go", script=[call("bash", {"command": "true"}, "c1"), call("bash", {"command": "true"}, "c2"),
                                         say("REPORT")])
        self.assertEqual(done.returncode, 1)
        self.assertEqual(done.stdout, "REPORT\n")
        self.assertIn("stopped after 2 model replies", done.stderr)
        self.assertIn("Write your final report now", self.model.requests[2]["messages"][-1]["content"][-1]["text"])

    def test_effort_is_sent_when_set(self):
        done = self.run_hx("go", script=[say("ok")])
        self.assertNotIn("output_config", self.model.requests[0])
        done = self.run_hx("go", script=[say("ok")], HX_EFFORT="max")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[1]["output_config"], {"effort": "max"})

    def test_a_claude_model_gets_its_effort_fallbacks_and_betas(self):
        done = self.run_hx("--model", "opus", "go", script=[say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        sent = self.model.requests[0]
        self.assertEqual(sent["model"], "claude-opus-5-5")
        self.assertEqual(sent["output_config"], {"effort": "medium"})
        self.assertEqual(sent["fallbacks"], "default")
        self.assertEqual(sent["thinking"], {"type": "adaptive", "display": "updates",
                                            "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
        self.assertEqual(self.model.headers[0]["anthropic-beta"], "thinking-binding-controls-2026-08-01,"
                         "thinking-display-updates-2026-08-18,server-side-fallback-2026-07-01,compact-2026-09-04")
        # Two cache breakpoints: one on the system prompt, which outlives a compaction, and the automatic one.
        self.assertEqual(sent["cache_control"], {"type": "ephemeral"})
        self.assertEqual(len(sent["system"]), 1)
        self.assertEqual(sent["system"][0]["cache_control"], {"type": "ephemeral"})
        self.assertIn("coding agent", sent["system"][0]["text"])
        # Haiku 5.5 has no server-side fallback; HX_EFFORT still wins.
        done = self.run_hx("go", script=[say("ok")], HX_MODEL="haiku", HX_EFFORT="low")
        self.assertEqual(done.returncode, 0, done.stderr)
        sent = self.model.requests[1]
        self.assertEqual(sent["model"], "claude-haiku-5-5")
        self.assertEqual(sent["output_config"], {"effort": "low"})
        self.assertNotIn("fallbacks", sent)
        self.assertEqual(self.model.headers[1]["anthropic-beta"], "thinking-binding-controls-2026-08-01,compact-2026-09-04")
        # Another model gets the plain request.
        done = self.run_hx("--model", "deepseek-flash", "go", script=[say("ok")])
        self.assertEqual(self.model.requests[2]["thinking"], {"type": "adaptive"})
        self.assertNotIn("fallbacks", self.model.requests[2])
        self.assertNotIn("anthropic-beta", self.model.headers[2])
        # DeepSeek caches by itself and ignores cache_control: the system prompt goes as the plain string.
        self.assertIsInstance(self.model.requests[2]["system"], str)

    def test_the_model_follows_the_key_that_is_set(self):
        show = ["--run", "-"]
        script = "print(host.env('HX_MODEL'))"
        done = self.run_hx(*show, stdin=script, HX_API_URL="", HX_MODEL="", DEEPSEEK_API_KEY="", ANTHROPIC_API_KEY="k")
        self.assertEqual(done.stdout, "claude-opus-5-5\n", done.stderr)
        done = self.run_hx(*show, stdin=script, HX_API_URL="", HX_MODEL="", DEEPSEEK_API_KEY="d", ANTHROPIC_API_KEY="k")
        self.assertEqual(done.stdout, "deepseek-flash\n", done.stderr)
        done = self.run_hx(*show, stdin=script, HX_MODEL="sonnet")
        self.assertEqual(done.stdout, "claude-sonnet-5-5\n", done.stderr)

    def test_what_a_declining_model_wrote_before_a_fallback_is_not_sent_back(self):
        fallback = {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-5"}}
        thinking = {"type": "thinking", "thinking": "", "signature": "sig"}
        done = self.run_hx("--model", "opus", "go", script=[
            reply([thinking, use("bash", {"command": "echo no"}, "c0"), fallback, thinking,
                   use("bash", {"command": "echo yes"}, "c1")], stop="tool_use"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("claude-opus-5-5 declined; claude-opus-5 continued", done.stderr)
        self.assertEqual(self.model.requests[1]["messages"][1]["content"],
                         [fallback, thinking, use("bash", {"command": "echo yes"}, "c1")])
        self.assertEqual(self.tool_results(), ["yes\n[exit 0]"])

    def test_progress_updates_are_shown_for_a_claude_model(self):
        update = {"type": "thinking", "thinking": "Looking at the tests next.", "signature": "sig"}
        done = self.run_hx("--model", "sonnet", "go", script=[reply([update, text("ok")])])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("Looking at the tests next.", done.stderr)
        self.assertEqual(done.stdout, "ok\n")
        done = self.run_hx("--model", "deepseek-flash", "go", script=[reply([update, text("ok")])])
        self.assertNotIn("Looking at the tests next.", done.stderr)

    def test_a_refusal_names_its_category(self):
        refused = reply([], stop="refusal")
        refused[1]["stop_details"] = {"type": "refusal", "category": "cyber", "explanation": None}
        done = self.run_hx("--model", "opus", "go", script=[refused])
        self.assertIn("[the model declined to answer (cyber)]", done.stderr)

    def test_the_task_can_come_from_stdin(self):
        done = self.run_hx(stdin="line one\nline two\n", script=[say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[0]["messages"][0]["content"], "line one\nline two")

    def test_no_task_is_an_error(self):
        done = self.run_hx(stdin="")
        self.assertEqual(done.returncode, 1)
        self.assertIn("no task", done.stderr)

    # --- the command line ---

    def test_help_and_version_go_to_stdout(self):
        done = self.run_hx("--help")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("examples:", done.stdout)
        self.assertEqual(done.stderr, "")
        done = self.run_hx("--version")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertRegex(done.stdout, r"^hx \d+\.\d+\.\d+\n$")

    def test_a_misspelt_option_gets_a_suggestion(self):
        done = self.run_hx("--no-sandbx", "go")
        self.assertEqual(done.returncode, 2)
        self.assertIn("unknown option --no-sandbx; did you mean --no-sandbox?", done.stderr)
        self.assertIn("hx --help", done.stderr)
        self.assertEqual(self.model.requests, [])
        done = self.run_hx("--ro")
        self.assertEqual(done.returncode, 2)
        self.assertIn("--ro needs a value", done.stderr)
        done = self.run_hx("--quiet=yes", "go")
        self.assertEqual(done.returncode, 2)
        self.assertIn("--quiet takes no value", done.stderr)

    def test_model_flag_and_equals_form(self):
        done = self.run_hx("--model=from-flag", "go", script=[say("ok")], HX_MODEL="from-env")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[0]["model"], "from-flag")
        done = self.run_hx("--model", "second", "go", script=[say("ok")])
        self.assertEqual(self.model.requests[1]["model"], "second")

    def test_quiet_leaves_out_progress_but_not_the_answer(self):
        done = self.run_hx("-q", "go", script=[call("bash", {"command": "true"}), say("the answer")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "the answer\n")
        self.assertEqual(done.stderr, "")
        loud = self.run_hx("go", script=[call("bash", {"command": "true"}), say("the answer")])
        self.assertIn("bash true", loud.stderr)

    def test_a_script_can_come_from_stdin(self):
        done = self.run_hx("--run", "-", "a", "-b", stdin="print(table.concat(host.args, ' '))")
        self.assertEqual((done.returncode, done.stdout), (0, "a -b\n"), done.stderr)

    def test_a_cut_off_reply_is_continued(self):
        done = self.run_hx("go", script=[say("part one", stop="max_tokens"), say("part two")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "part one\npart two\n")
        self.assertIn("cut off", self.model.requests[1]["messages"][-1]["content"])

    def test_a_cut_off_tool_call_does_not_run(self):
        done = self.run_hx("go", script=[call("bash", {"command": "touch ran.txt"}, stop="max_tokens"), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("cut off by the output limit", self.tool_results()[0])
        self.assertFalse(os.path.exists(os.path.join(self.project, "ran.txt")))

    def test_a_failed_request_is_retried(self):
        done = self.run_hx("go", script=[(500, {"error": "busy"}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(self.model.requests), 2)
        self.assertEqual(self.model.requests[0], self.model.requests[1])

    def test_a_rejected_request_ends_the_run(self):
        done = self.run_hx("go", script=[(400, {"error": {"message": "no such model"}})])
        self.assertEqual(done.returncode, 1)
        self.assertIn("no such model", done.stderr)
        self.assertEqual(len(self.model.requests), 1)

    def test_a_long_conversation_is_replaced_by_its_summary(self):
        done = self.run_hx("the task", script=[call("bash", {"command": "echo big"}, input_tokens=200000),
                                               say("SUMMARY TEXT"), say("finished")])
        self.assertEqual(done.returncode, 0, done.stderr)
        _, ask, after = self.model.requests
        # The request for the summary rides on the message that carries the tool result, after the result.
        self.assertEqual([m["role"] for m in ask["messages"]], ["user", "assistant", "user"])
        self.assertEqual([b["type"] for b in ask["messages"][2]["content"]], ["tool_result", "text"])
        self.assertIn("replaced by a summary", ask["messages"][2]["content"][1]["text"])
        self.assertEqual([m["role"] for m in after["messages"]], ["user"])
        self.assertIn("SUMMARY TEXT", after["messages"][0]["content"])
        self.assertEqual(after["system"], ask["system"])
        self.assertEqual(done.stdout, "finished\n")  # the summary itself is not printed as an answer

    def test_a_claude_model_compacts_on_the_server(self):
        block = {"type": "compaction", "content": "SUMMARY TEXT", "signature": "sig-c"}
        compacted = (200, {"id": "msg_c", "type": "message", "role": "assistant", "content": [block],
                           "stop_reason": "compaction", "stop_sequence": None,
                           "usage": {"input_tokens": 0, "output_tokens": 0,
                                     "iterations": [{"type": "compaction", "input_tokens": 300, "output_tokens": 70}]}})
        done = self.run_hx("--model", "opus", "the task", script=[
            call("bash", {"command": "echo big"}, input_tokens=200000), compacted, say("finished")])
        self.assertEqual(done.returncode, 0, done.stderr)
        first, ask, after = self.model.requests
        # The same request as the conversation's, with `compaction` and the loop's own summarization prompt.
        self.assertEqual(ask["compaction"]["type"], "summarize")
        self.assertIn("## Next step", ask["compaction"]["instructions"])
        self.assertEqual(ask["messages"][0], first["messages"][0])
        self.assertEqual([b["type"] for b in ask["messages"][2]["content"]], ["tool_result"])
        for key in ("system", "tools", "thinking", "output_config", "model"):
            self.assertEqual(ask[key], first[key])
        self.assertIn("compact-2026-09-04", self.model.headers[1]["anthropic-beta"].split(","))
        # The block goes first, exactly as it came, and stands for everything it summarized.
        self.assertEqual(len(after["messages"]), 1)
        self.assertEqual(after["messages"][0]["role"], "user")
        self.assertEqual(after["messages"][0]["content"][0], block)
        self.assertIn("continue the work", after["messages"][0]["content"][1]["text"])
        self.assertNotIn("compaction", after)
        self.assertIn("compact-2026-09-04", self.model.headers[2]["anthropic-beta"].split(","))
        self.assertEqual(done.stdout, "finished\n")
        # What the summary cost is counted from usage.iterations.
        self.assertIn("3 replies", done.stderr)
        self.assertIn("80 out]", done.stderr)

    def test_a_claude_compaction_without_a_summary_keeps_the_conversation(self):
        empty = (200, {"id": "msg_c", "type": "message", "role": "assistant", "content": [], "stop_reason": "max_tokens",
                       "stop_sequence": None, "usage": {"input_tokens": 0, "output_tokens": 0}})
        done = self.run_hx("--model", "opus", "the task", script=[
            call("bash", {"command": "echo big"}, input_tokens=200000), empty, say("finished")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("no summary came back (max_tokens)", done.stderr)
        _, ask, after = self.model.requests
        self.assertEqual(after["messages"], ask["messages"])
        self.assertNotIn("compaction", after)

    # --- the walls ---

    def test_the_model_cannot_leave_the_project(self):
        with open(os.path.join(self.home, "secret"), "w") as f:
            f.write("TOP-SECRET")
        outside = os.path.join(self.root, "outside.txt")
        done = self.run_hx("go", script=[
            call("bash", {"command": "cat ~/secret; ls ~"}, "c1"),
            call("read", {"path": os.path.join(self.home, "secret")}, "c2"),
            call("bash", {"command": f"echo x > {outside}; echo x > ~/planted; mkdir /etc/hx-test"}, "c3"),
            call("write", {"path": outside, "content": "x"}, "c4"),
            call("lua", {"code": f"return host.read('{self.home}/secret')"}, "c5"),
            call("bash", {"command": "ls /dev/pts; cat /dev/pts/0 /dev/tty"}, "c6"),
            call("bash", {"command": "echo in > inside.txt && echo t > $TMPDIR/t && cat inside.txt $TMPDIR/t && ls /usr >/dev/null && echo fine"}, "c7"),
            call("bash", {"command": "echo out > /dev/stdout; echo err > /dev/stderr; cat <(echo sub); echo x > /dev/null; head -c 3 /dev/zero | wc -c"}, "c8"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        results = self.tool_results()
        self.assertEqual(len(results), 8)
        self.assertNotIn("TOP-SECRET", json.dumps(self.model.requests))
        for i in range(5):
            self.assertIn("Permission denied", results[i], i)
        self.assertIn("/dev/pts': Permission denied", results[5])  # the user's terminals are out of reach
        self.assertNotIn("\n[exit 0]", results[5])
        self.assertEqual(results[6], "in\nt\nfine\n[exit 0]")
        self.assertEqual(results[7], "out\nerr\nsub\n3\n[exit 0]")  # the ordinary plumbing still works
        self.assertFalse(os.path.exists(outside))
        self.assertFalse(os.path.exists(os.path.join(self.home, "planted")))

    def test_ro_and_rw_open_more(self):
        extra_ro = os.path.join(self.root, "ro")
        extra_rw = os.path.join(self.root, "rw")
        os.makedirs(extra_ro)
        os.makedirs(extra_rw)
        with open(os.path.join(extra_ro, "f"), "w") as f:
            f.write("readable")
        done = self.run_hx("--ro", extra_ro, "--rw", extra_rw, "go", script=[
            call("bash", {"command": f"cat {extra_ro}/f; echo x > {extra_ro}/g; echo y > {extra_rw}/g && cat {extra_rw}/g"}),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        result = self.tool_results()[0]
        self.assertIn("readable", result)
        self.assertIn(f"{extra_ro}/g: Permission denied", result)
        self.assertIn("\ny\n", result)

    def test_git_still_reads_its_config(self):
        with open(os.path.join(self.home, ".gitconfig"), "w") as f:
            f.write("[user]\n\tname = Test Owner\n")
        self.run_hx("go", script=[call("bash", {"command": "git config user.name"}), say("ok")])
        self.assertEqual(self.tool_results(), ["Test Owner\n[exit 0]"])

    def test_trust_roots_named_in_the_environment_are_readable(self):
        bundle = os.path.join(self.home, "ca.pem")
        with open(bundle, "w") as f:
            f.write("TRUST-ROOT")
        with open(os.path.join(self.home, "beside.pem"), "w") as f:
            f.write("NOT-THIS")
        done = self.run_hx("go", REQUESTS_CA_BUNDLE=bundle, script=[
            call("bash", {"command": f"cat $REQUESTS_CA_BUNDLE; cat {self.home}/beside.pem"}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        result = self.tool_results()[0]
        self.assertIn("TRUST-ROOT", result)
        self.assertIn("beside.pem: Permission denied", result)
        self.assertNotIn("NOT-THIS", result)

    def test_the_home_directory_is_not_a_project(self):
        done = subprocess.run([HX, "go"], cwd=self.home, env=self.env(), capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 2)
        self.assertIn("refusing to treat", done.stderr)
        self.assertEqual(self.model.requests, [])

    def test_a_directory_that_holds_the_home_directory_is_not_a_project(self):
        done = subprocess.run([HX, "go"], cwd=self.root, env=self.env(), capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 2)
        self.assertIn("refusing to treat", done.stderr)
        self.assertEqual(self.model.requests, [])
        # A sibling of the home directory is an ordinary project.
        sibling = os.path.join(self.root, "home2")
        os.makedirs(sibling)
        self.model.script = [say("ok")]
        done = subprocess.run([HX, "go"], cwd=sibling, env=self.env(), capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stderr)

    def test_the_api_key_reaches_the_endpoint_and_nothing_else(self):
        done = self.run_hx("go", OTHER_SECRET_TOKEN="hunter2-token", GIT_CONFIG_KEY_0="not-a-secret", script=[
            call("bash", {"command": "env; tr '\\0' '\\n' < /proc/$PPID/environ; cat /proc/$PPID/cmdline"}, "c1"),
            call("lua", {"code": "local t = {} for k, v in host do t[#t + 1] = k .. '=' .. tostring(v) end "
                                 "return table.concat(t, ' '), host.env('HX_TEST_KEY'), host.env('OTHER_SECRET_TOKEN')"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.headers[0]["x-api-key"], KEY)
        self.assertEqual(self.model.headers[0]["anthropic-version"], "2023-06-01")
        sent = json.dumps(self.model.requests)
        self.assertNotIn(KEY, sent)
        self.assertNotIn("hunter2-token", sent)
        self.assertIn("GIT_CONFIG_KEY_0=not-a-secret", self.tool_results()[0])  # only secrets are removed

    def test_an_inherited_descriptor_does_not_pass_the_wall(self):
        outside = os.path.join(self.root, "outside.txt")
        with open(outside, "w") as f:
            f.write("FROM-OUTSIDE")
        fd = os.open(outside, os.O_RDONLY)
        try:
            self.model.script = [call("bash", {"command": f"cat <&{fd}"}), say("ok")]
            done = subprocess.run([HX, "go"], cwd=self.project, env=self.env(), pass_fds=(fd,), capture_output=True,
                                  text=True, timeout=60)
        finally:
            os.close(fd)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("Bad file descriptor", self.tool_results()[0])
        self.assertNotIn("FROM-OUTSIDE", json.dumps(self.model.requests))

    def test_a_unix_socket_outside_cannot_be_reached(self):
        path = os.path.join(self.root, "outside.sock")
        connect = f"import socket; s = socket.socket(socket.AF_UNIX); s.connect('{path}'); print('connected')"
        pair = "import socket; a, b = socket.socketpair(); a.send(b'pair'); print(b.recv(4).decode())"
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(path)
            listener.listen()
            done = self.run_hx("go", script=[call("bash", {"command": f'python3 -c "{connect}"'}, "c1"),
                                             call("bash", {"command": f'python3 -c "{pair}"'}, "c2"), say("ok")])
            listener.setblocking(False)
            with self.assertRaises(BlockingIOError):  # nothing is waiting to be accepted
                listener.accept()
        self.assertEqual(done.returncode, 0, done.stderr)
        refused, paired = self.tool_results()
        # Before Landlock ABI 9 the filter refuses the socket; from ABI 9 Landlock refuses the connection.
        self.assertIn("PermissionError", refused)
        self.assertNotIn("[exit 0]", refused)
        self.assertEqual(paired, "pair\n[exit 0]")
        self.assertNotIn("unix sockets", done.stderr)  # the wall is whole, so there is no note

    def test_a_datagram_pair_cannot_reach_a_socket_outside(self):
        # A datagram socket is never tied to its peer: one end of a pair could send to any path.
        path = os.path.join(self.root, "outside-datagram.sock")
        send = (f"import socket; a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM); "
                f"a.sendto(b'leak', '{path}'); print('sent')")
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as listener:
            listener.bind(path)
            done = self.run_hx("go", script=[call("bash", {"command": f'python3 -c "{send}"'}), say("ok")])
            listener.setblocking(False)
            with self.assertRaises(BlockingIOError):  # nothing arrived
                listener.recv(16)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("PermissionError", self.tool_results()[0])

    def test_unix_sockets_opens_sockets_inside_the_project(self):
        script = ("import socket; s = socket.socket(socket.AF_UNIX); s.bind('in.sock'); s.listen(); "
                  "c = socket.socket(socket.AF_UNIX); c.connect('in.sock'); a, _ = s.accept(); c.send(b'local'); "
                  "print(a.recv(5).decode())")
        done = self.run_hx("--unix-sockets", "go", script=[call("bash", {"command": f'python3 -c "{script}"'}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results(), ["local\n[exit 0]"])

    def test_keep_env_keeps_a_variable(self):
        self.run_hx("--keep-env", "KEPT_TOKEN", "go", KEPT_TOKEN="kept-value", script=[
            call("bash", {"command": "echo $KEPT_TOKEN"}), say("ok")])
        self.assertEqual(self.tool_results(), ["kept-value\n[exit 0]"])

    def test_a_secret_is_known_by_the_end_of_its_name(self):
        secrets = {"PGPASSWORD": "s1", "OPENAI_APIKEY": "s2", "GITHUB_PAT": "s3", "MYSQL_PWD": "s4", "MY_API_KEY": "s5",
                   "AUTHTOKEN": "s6"}
        kept = {"GIT_CONFIG_KEY_0": "p1", "PATH_PAT_X": "p2", "COMPAT": "p3", "OLDPWD": "/", "KEPT_PAT": "s7"}
        done = self.run_hx("--keep-env", "KEPT_PAT", "--run", "-", stdin="print(host.sh('env').out)",
                           **secrets, **kept)
        self.assertEqual(done.returncode, 0, done.stderr)
        shown = dict(line.split("=", 1) for line in done.stdout.splitlines() if "=" in line)
        self.assertEqual([name for name in secrets if name in shown], [])
        self.assertEqual({name: shown.get(name) for name in kept}, kept)
        self.assertIn("PWD", shown)

    # --- processes ---

    def test_a_command_that_runs_too_long_is_killed_with_its_children(self):
        started = time.monotonic()
        self.run_hx("go", script=[
            call("bash", {"command": "(sleep 300; echo late) & echo $! > child.pid; sleep 300", "timeout": 1}), say("ok")])
        self.assertLess(time.monotonic() - started, 20)
        self.assertIn("[timed out and was killed]", self.tool_results()[0])
        self.assert_gone(int(read(self.project, "child.pid")))

    def frozen_cgroup(self):
        """A cgroup v1 freezer group, frozen once a process is put in it. A frozen process ignores SIGKILL until it
        is thawed, as one stuck in uninterruptible I/O does until the I/O returns."""
        group = os.path.join("/sys/fs/cgroup/freezer", os.path.basename(self.root))
        try:
            os.mkdir(group)
        except OSError as e:
            self.skipTest(f"no cgroup v1 freezer to make a process unkillable: {e}")

        def remove():
            with open(os.path.join(group, "freezer.state"), "w") as f:
                f.write("THAWED")
            for _ in range(100):
                if not read(group, "cgroup.procs").strip():
                    break
                time.sleep(0.05)
            os.rmdir(group)

        self.addCleanup(remove)
        return group

    def test_a_command_stuck_past_its_kill_holds_up_neither_the_call_nor_the_exit(self):
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("local r = host.sh('echo $$ > sh.pid; exec sleep 300', {timeout = 3})\n"
                    "print(r.timed_out, r.signal, r.code)\n")
        process = subprocess.Popen([HX, "--run", "s.luau"], cwd=self.project, env=self.env(),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(kill_quietly, process.pid)
        pid = self.wait_for_pid_file("sh.pid")
        group = self.frozen_cgroup()  # its cleanup runs first, so a binary that hangs here is let go
        with open(os.path.join(group, "cgroup.procs"), "w") as f:
            f.write(str(pid))
        with open(os.path.join(group, "freezer.state"), "w") as f:
            f.write("FROZEN")
        started = time.monotonic()
        out, err = process.communicate(timeout=20)
        self.assertLess(time.monotonic() - started, 10)
        self.assertEqual((process.returncode, out), (0, "true\t9\tnil\n"), err)
        with open(os.path.join(group, "freezer.state"), "w") as f:
            f.write("THAWED")
        self.assert_gone(pid)

    def test_a_background_process_lives_until_hx_exits(self):
        done = self.run_hx("go", script=[
            call("bash", {"command": "sleep 300 > /dev/null 2>&1 & echo $! > bg.pid"}, "c1"),
            call("bash", {"command": "kill -0 $(cat bg.pid) && echo still running"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results()[1], "still running\n[exit 0]")
        self.assert_gone(int(read(self.project, "bg.pid")))

    def test_a_daemon_lives_until_hx_exits(self):
        for flags in [(), ("--no-sandbox",)]:
            daemon = "setsid sh -c 'echo $$ > daemon.pid; exec sleep 300' > /dev/null 2>&1 < /dev/null &"
            done = self.run_hx(*flags, "go", script=[
                call("bash", {"command": daemon + " while [ ! -s daemon.pid ]; do sleep 0.05; done"}, "c1"),
                call("bash", {"command": "ps -o sid= -p $$ $(cat daemon.pid)"}, "c2"),
                say("ok")])
            self.assertEqual(done.returncode, 0, done.stderr)
            sessions = self.tool_results()[1].split()[:2]
            self.assertNotEqual(sessions[0], sessions[1], "the daemon left the command's session")
            # The model is told, so it does not report a server as left running.
            self.assertIn("nothing you start outlives it", self.model.requests[0]["tools"][0]["description"])
            self.assert_gone(int(read(self.project, "daemon.pid")))
            os.remove(os.path.join(self.project, "daemon.pid"))

    def wait_for_pid_file(self, name):
        path = os.path.join(self.project, name)
        for _ in range(200):
            if os.path.exists(path) and read(path).strip():
                return int(read(path))
            time.sleep(0.05)
        self.fail(f"{name} never appeared")

    def test_sigint_stops_the_command_and_the_run(self):
        self.model.script = [call("bash", {"command": "echo $$ > cmd.pid; sleep 300"}), say("never")]
        process = subprocess.Popen([HX, "go"], cwd=self.project, env=self.env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        pid = self.wait_for_pid_file("cmd.pid")
        process.send_signal(signal.SIGINT)
        process.communicate(timeout=20)
        self.assertEqual(process.returncode, 130)
        self.assert_gone(pid)
        self.assertEqual(len(self.model.requests), 1)

    def test_an_interrupted_turn_leaves_a_conversation_that_continues(self):
        self.model.script = [say("first answer"), call("bash", {"command": "echo $$ > cmd.pid; sleep 300"}),
                             say("after the interrupt")]
        master, slave = pty.openpty()
        process = subprocess.Popen([HX], cwd=self.project, env=self.env(), stdin=slave, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, text=True)
        os.close(slave)
        try:
            os.write(master, b"hello\n")
            self.assertEqual(process.stdout.readline(), "first answer\n")
            os.write(master, b"run it\n")
            pid = self.wait_for_pid_file("cmd.pid")
            process.send_signal(signal.SIGINT)
            self.assert_gone(pid)
            os.write(master, b"go on\n")
            self.assertEqual(process.stdout.readline(), "after the interrupt\n")
            os.write(master, b"\x04")  # Ctrl-D
            process.stdout.close()
            self.assertEqual(process.wait(timeout=20), 0)
        finally:
            process.kill()
            os.close(master)
        messages = self.model.requests[2]["messages"]
        self.assertEqual([m["role"] for m in messages], ["user", "assistant", "user", "assistant", "user"])
        self.assertEqual(messages[4]["content"], [
            {"type": "tool_result", "tool_use_id": "call_1", "content": "error: interrupted by the user", "is_error": True},
            {"type": "text", "text": "go on"},
        ])

    def test_an_interrupted_request_leaves_a_conversation_that_continues(self):
        self.model.script = [("hang", 30), say("after the interrupt")]
        master, slave = pty.openpty()
        process = subprocess.Popen([HX], cwd=self.project, env=self.env(), stdin=slave, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, text=True)
        os.close(slave)
        try:
            os.write(master, b"hello\n")
            self.assertTrue(self.model.hanging.wait(timeout=20))
            started = time.monotonic()
            process.send_signal(signal.SIGINT)
            os.write(master, b"again\n")
            self.assertEqual(process.stdout.readline(), "after the interrupt\n")
            self.assertLess(time.monotonic() - started, 10)  # the request was cut, not waited out
            os.write(master, b"\x04")
            process.stdout.close()
            self.assertEqual(process.wait(timeout=20), 0)
        finally:
            process.kill()
            os.close(master)
        # The interrupted message was never answered, so the next one joins it: roles still alternate.
        self.assertEqual(self.model.requests[1]["messages"], [
            {"role": "user", "content": [{"type": "text", "text": "hello"}, {"type": "text", "text": "again"}]}])

    def test_a_failed_turn_returns_to_the_prompt(self):
        self.model.script = [call("bash", {"command": "echo ran"}), (400, {"error": {"message": "no such model"}}),
                             say("after the failure")]
        master, slave = pty.openpty()
        with open(os.path.join(self.root, "stderr"), "w+") as errors:
            process = subprocess.Popen([HX], cwd=self.project, env=self.env(), stdin=slave, stdout=subprocess.PIPE,
                                       stderr=errors, text=True)
            os.close(slave)
            try:
                os.write(master, b"hello\n")
                os.write(master, b"again\n")  # read once the failed turn is over
                self.assertEqual(process.stdout.readline(), "after the failure\n")
                os.write(master, b"\x04")
                process.stdout.close()
                self.assertEqual(process.wait(timeout=20), 0)
            finally:
                process.kill()
                os.close(master)
            errors.seek(0)
            self.assertIn("no such model", errors.read())
        self.assertEqual(len(self.model.requests), 3)
        messages = self.model.requests[2]["messages"]
        self.assertEqual([m["role"] for m in messages], ["user", "assistant", "user"])
        self.assertEqual(messages[2]["content"], [
            {"type": "tool_result", "tool_use_id": "call_1", "content": "ran\n[exit 0]"},
            {"type": "text", "text": "again"},
        ])

    def test_a_pasted_message_is_one_message(self):
        self.model.script = [say("one"), say("two"), say("three")]
        master, slave = pty.openpty()
        with open(os.path.join(self.root, "stderr"), "w+b") as errors:
            process = subprocess.Popen([HX], cwd=self.project, env=self.env(), stdin=slave, stdout=subprocess.PIPE,
                                       stderr=errors, text=True)
            os.close(slave)
            try:
                os.write(master, b"\x1b[200~first line\nsecond line\nthird\x1b[201~\n")
                self.assertEqual(process.stdout.readline(), "one\n")
                self.assertEqual(len(self.model.requests), 1)
                os.write(master, b"plain\n")
                self.assertEqual(process.stdout.readline(), "two\n")
                os.write(master, b"before \x1b[200~a\nb\x1b[201~ after\n")
                self.assertEqual(process.stdout.readline(), "three\n")
                os.write(master, b"\x04")
                process.stdout.close()
                self.assertEqual(process.wait(timeout=20), 0)
            finally:
                process.kill()
                os.close(master)
            errors.seek(0)
            written = errors.read()
        self.assertEqual(len(self.model.requests), 3)
        self.assertEqual(self.model.requests[0]["messages"], [{"role": "user", "content": "first line\nsecond line\nthird"}])
        self.assertEqual(self.model.requests[1]["messages"][2], {"role": "user", "content": "plain"})
        self.assertEqual(self.model.requests[2]["messages"][4], {"role": "user", "content": "before a\nb after"})
        # Paste is turned on at the prompt and off when the session ends.
        self.assertIn(b"\x1b[?2004h> ", written)
        self.assertGreater(written.rfind(b"\x1b[?2004l"), written.rfind(b"\x1b[?2004h"))

    def test_a_paste_cut_short_by_the_end_of_input_is_sent(self):
        self.model.script = [say("got it")]
        master, slave = pty.openpty()
        process = subprocess.Popen([HX], cwd=self.project, env=self.env(), stdin=slave, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, text=True)
        os.close(slave)
        try:
            os.write(master, b"\x1b[200~cut\nshort\n\x04")
            self.assertEqual(process.stdout.readline(), "got it\n")
            process.stdout.close()
            self.assertEqual(process.wait(timeout=20), 0)
        finally:
            process.kill()
            os.close(master)
        self.assertEqual(self.model.requests[0]["messages"], [{"role": "user", "content": "cut\nshort"}])

    def test_a_stop_at_the_prompt_turns_paste_off(self):
        master, slave = pty.openpty()
        with open(os.path.join(self.root, "stderr"), "w+b") as errors:
            process = subprocess.Popen([HX], cwd=self.project, env=self.env(), stdin=slave, stdout=subprocess.DEVNULL,
                                       stderr=errors)
            os.close(slave)
            try:
                deadline = time.monotonic() + 20
                while "> " not in read(errors.name) and time.monotonic() < deadline:
                    time.sleep(0.05)
                os.write(master, b"\x1b[200~half a paste\n")
                time.sleep(0.3)  # let it block in the read of the next line
                process.send_signal(signal.SIGINT)
                self.assertEqual(process.wait(timeout=20), 130)
            finally:
                process.kill()
                os.close(master)
            errors.seek(0)
            written = errors.read()
        self.assertTrue(written.endswith(b"\x1b[?2004l\n"), written)
        self.assertEqual(self.model.requests, [])

    def test_a_tool_cannot_swallow_an_interrupt(self):
        code = "local ok, err = pcall(host.sh, 'echo $$ > cmd.pid; sleep 300') return 'swallowed: ' .. tostring(err)"
        self.model.script = [call("lua", {"code": code}), say("after the interrupt")]
        master, slave = pty.openpty()
        process = subprocess.Popen([HX], cwd=self.project, env=self.env(), stdin=slave, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, text=True)
        os.close(slave)
        try:
            os.write(master, b"run it\n")
            pid = self.wait_for_pid_file("cmd.pid")
            process.send_signal(signal.SIGINT)
            self.assert_gone(pid)
            os.write(master, b"go on\n")
            self.assertEqual(process.stdout.readline(), "after the interrupt\n")
            os.write(master, b"\x04")
            process.stdout.close()
            self.assertEqual(process.wait(timeout=20), 0)
        finally:
            process.kill()
            os.close(master)
        self.assertEqual(self.model.requests[1]["messages"][2]["content"], [
            {"type": "tool_result", "tool_use_id": "call_1", "content": "error: interrupted by the user", "is_error": True},
            {"type": "text", "text": "go on"},
        ])

    def run_interrupted(self, source):
        """Runs a script, interrupts it once the command it starts has written cmd.pid, and returns the result."""
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write(source)
        process = subprocess.Popen([HX, "--run", "s.luau"], cwd=self.project, env=self.env(), stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        pid = self.wait_for_pid_file("cmd.pid")
        process.send_signal(signal.SIGINT)
        out, _ = process.communicate(timeout=20)
        self.assert_gone(pid)
        return process.returncode, out

    def test_pcall_cannot_swallow_an_interrupt(self):
        code, out = self.run_interrupted("print(pcall(host.sh, 'echo $$ > cmd.pid; sleep 20'))\nprint('after')\n")
        self.assertEqual((code, out), (130, ""))

    def test_host_try_is_where_an_interrupt_ends(self):
        code, out = self.run_interrupted(
            "print(host.try(host.sh, 'echo $$ > cmd.pid; sleep 20'))\nprint(host.sh('echo carries on').out)\n")
        self.assertEqual((code, out), (0, "false\tinterrupted\ncarries on\n\n"))

    def test_input_still_reads_after_an_interrupted_read(self):
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("print('ready')\nprint(host.try(host.input))\nprint(host.input())\n")
        process = subprocess.Popen([HX, "--run", "s.luau"], cwd=self.project, env=self.env(), stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.assertEqual(process.stdout.readline(), "ready\n")
        time.sleep(0.3)  # let it block in the read
        process.send_signal(signal.SIGINT)
        # Wait for the stop to end before writing: a line that arrives before hx wakes is read and then dropped.
        self.assertEqual(process.stdout.readline(), "false\tinterrupted\n")
        out, _ = process.communicate("a line\n", timeout=20)
        self.assertEqual((process.returncode, out), (0, "a line\n"))

    def test_an_inner_scope_does_not_end_an_interrupt(self):
        code, out = self.run_interrupted(
            "local inner\n"
            "local ok, err = host.try(function()\n"
            "  inner = { host.timed(30, host.sh, 'echo $$ > cmd.pid; sleep 20') }\n"
            "  host.sh('touch never.txt')\n"
            "end)\n"
            "print(ok, err, inner[1], inner[2])\n")
        self.assertEqual((code, out), (0, "false\tinterrupted\tfalse\tinterrupted\n"))
        self.assertFalse(os.path.exists(os.path.join(self.project, "never.txt")))

    def test_a_second_signal_kills_what_the_model_started(self):
        # table.sort without a comparison function runs in C, where no safepoint looks for a stop, so the first stop
        # is not handled for the seconds it takes and the second signal leaves at once.
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("host.sh(\"setsid sh -c 'echo $$ > daemon.pid; exec sleep 300' > /dev/null 2>&1 < /dev/null &\")\n"
                    "local t = string.split(string.rep('b,a,c,', 8000000), ',')\n"  # built in C too: quick
                    "host.sh(\"sh -c 'echo $$ > child.pid; exec sleep 300' > /dev/null 2>&1 &\")\n"
                    "table.sort(t)\n")
        process = subprocess.Popen([HX, "--run", "s.luau"], cwd=self.project, env=self.env(),
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(kill_quietly, process.pid)
        daemon = self.wait_for_pid_file("daemon.pid")
        for _ in range(600):  # building the list takes seconds under a sanitizer
            if os.path.exists(os.path.join(self.project, "child.pid")):
                break
            time.sleep(0.05)
        child = self.wait_for_pid_file("child.pid")  # written just before the sort starts
        for pid in (daemon, child):
            self.addCleanup(kill_quietly, pid)  # against a binary that leaves them behind
        time.sleep(0.3)  # let the sort start
        process.send_signal(signal.SIGINT)
        time.sleep(0.3)
        self.assertIsNone(process.poll(), "the first signal ended the run: the test did not get hx stuck")
        process.send_signal(signal.SIGINT)
        self.assertEqual(process.wait(timeout=20), 130)
        self.assert_gone(daemon)
        self.assert_gone(child)

    def test_without_the_sandbox_the_model_is_told_so(self):
        outside = os.path.join(self.root, "outside.txt")
        done = self.run_hx("--no-sandbox", "go", script=[call("bash", {"command": f"echo x > {outside} && echo wrote"}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results(), ["wrote\n[exit 0]"])
        self.assertIn("Nothing confines you", self.model.requests[0]["system"])
        self.assertNotIn("The kernel confines you", self.model.requests[0]["system"])

    def assert_gone(self, pid):
        for _ in range(100):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            try:  # a zombie still answers kill(0); it is gone once its state says so
                if read(f"/proc/{pid}/stat").rsplit(")", 1)[1].split()[0] == "Z":
                    return
            except FileNotFoundError:
                return
            time.sleep(0.05)
        self.fail(f"process {pid} is still alive")

    # --- files ---

    def test_a_write_that_fails_part_way_leaves_the_old_content(self):
        with open(os.path.join(self.project, "f.txt"), "w") as f:
            f.write("old\n")
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("print(host.write('f.txt', string.rep('x', 100000)))\nprint(table.concat(host.list('.'), ' '))\n")
        # A file size limit of 1 KiB makes the write fail after its first kilobyte, whoever owns what. SIGXFSZ is
        # ignored, so the write returns EFBIG instead of killing hx.
        done = subprocess.run(["bash", "-c", 'ulimit -f 1 && trap "" XFSZ && exec "$0" --run s.luau', HX],
                              cwd=self.project, env=self.env(), capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "nil\tf.txt: File too large\nf.txt s.luau\n")
        self.assertEqual(read(self.project, "f.txt"), "old\n")

    def test_a_replaced_file_keeps_its_mode(self):
        target = os.path.join(self.project, "f.txt")
        with open(target, "w") as f:
            f.write("old")
        os.chmod(target, 0o640)  # not what a new file gets
        before = os.stat(target)
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("print(host.write('f.txt', 'new'))\n")
        done = subprocess.run([HX, "--run", "s.luau"], cwd=self.project, env=self.env(), capture_output=True,
                              text=True, timeout=60)
        self.assertEqual((done.returncode, done.stdout), (0, "true\n"), done.stderr)
        after = os.stat(target)
        self.assertNotEqual(after.st_ino, before.st_ino, "the file was written in place, not replaced")
        self.assertEqual(read(target), "new")
        self.assertEqual(after.st_mode, before.st_mode)

    def test_a_write_killed_part_way_leaves_the_file_whole(self):
        # Each write takes long enough that a kill lands inside one. The temporary file may be left beside it.
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("local big = string.rep('x', 32 * 1024 * 1024)\n"
                    "while true do host.write('f.txt', big) end\n")
        process = subprocess.Popen([HX, "--run", "s.luau"], cwd=self.project, env=self.env(),
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(kill_quietly, process.pid)
        for _ in range(200):
            if os.path.exists(os.path.join(self.project, "f.txt")):
                break
            time.sleep(0.05)
        time.sleep(0.5)
        process.kill()
        process.wait(timeout=20)
        self.assertEqual(os.path.getsize(os.path.join(self.project, "f.txt")), 32 * 1024 * 1024)

    def test_a_lone_rw_file_is_written_in_place(self):
        # Its directory is outside the wall, so no temporary file can be made beside it.
        outside = os.path.join(self.root, "lone.txt")
        with open(outside, "w") as f:
            f.write("old")
        done = self.run_hx("--rw", outside, "go", script=[call("write", {"path": outside, "content": "new"}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results(), [f"wrote 3 bytes to {outside}"])
        self.assertEqual(read(outside), "new")
        self.assertEqual(sorted(os.listdir(self.root)), ["home", "lone.txt", "project"])

    def as_ordinary_user(self):
        """The command prefix that runs hx as a user file permissions apply to. Root is not one, so as root the
        test directories are handed to nobody and hx runs as nobody."""
        if os.geteuid() != 0:
            return []
        os.chmod(self.root, 0o755)
        for path in (self.project, self.home):
            os.chown(path, 65534, 65534)
        return ["setpriv", "--reuid=65534", "--regid=65534", "--clear-groups"]

    def test_a_read_only_file_is_not_replaced(self):
        # Renaming a new file over it would not ask the file's own permission, so the write must refuse as before.
        become = self.as_ordinary_user()
        target = os.path.join(self.project, "ro.txt")
        with open(target, "w") as f:
            f.write("old")
        os.chmod(target, 0o444)
        owner = os.stat(self.project)
        os.chown(target, owner.st_uid, owner.st_gid)  # the same owner and group, so only the mode stands in the way
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("print(host.write('ro.txt', 'new'))\n")
        done = subprocess.run([*become, HX, "--run", "s.luau"], cwd=self.project, env=self.env(TMPDIR=self.home),
                              capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "nil\tro.txt: Permission denied\n")
        self.assertEqual(read(target), "old")
        self.assertEqual(sorted(os.listdir(self.project)), ["ro.txt", "s.luau"])

    def test_a_trust_root_out_of_reach_does_not_stop_hx(self):
        # The variable names a file in a directory this user cannot enter: there is nothing to allow, so no rule.
        become = self.as_ordinary_user()
        locked = os.path.join(self.root, "locked")
        os.makedirs(locked)
        with open(os.path.join(locked, "ca.pem"), "w") as f:
            f.write("roots")
        os.chmod(locked, 0o700 if os.geteuid() == 0 else 0)
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("print(host.sandboxed)\n")
        env = self.env(TMPDIR=self.home, REQUESTS_CA_BUNDLE=os.path.join(locked, "ca.pem"))
        for name in ("SSL_CERT_FILE", "SSL_CERT_DIR", "CURL_CA_BUNDLE", "NODE_EXTRA_CA_CERTS", "GIT_SSL_CAINFO",
                     "CARGO_HTTP_CAINFO", "AWS_CA_BUNDLE", "NIX_SSL_CERT_FILE"):
            env.pop(name, None)
        try:
            done = subprocess.run([*become, HX, "--run", "s.luau"], cwd=self.project, env=env, capture_output=True,
                                  text=True, timeout=60)
        finally:
            os.chmod(locked, 0o700)  # so that tearDown can remove it
        self.assertEqual((done.returncode, done.stdout), (0, "true\n"), done.stderr)

    def test_a_fifo_is_read_and_written_within_the_time_limit(self):
        script = ("host.sh('mkfifo fifo')\n"
                  "print(host.timed(1, host.read, 'fifo'))\n"
                  "print(host.timed(1, host.write, 'fifo', 'x'))\n"
                  "host.sh(\"(sleep 0.2; printf 'one '; sleep 0.2; printf two) > fifo &\")\n"
                  "print(host.timed(10, host.read, 'fifo'))\n"
                  "host.sh('cat fifo > got.txt &')\n"
                  "print(host.timed(10, host.write, 'fifo', string.rep('y', 200000)))\n"
                  "host.sh('(exec 3<fifo; sleep 30) & sleep 0.2')\n"  # a reader that never reads: the pipe fills
                  "print(host.timed(1, host.write, 'fifo', string.rep('y', 200000)))\n"
                  "print(host.read('/dev/null'), host.write('/dev/null', 'x'))\n")
        started = time.monotonic()
        done = self.run_hx("--run", "-", stdin=script)
        self.assertEqual(done.returncode, 0, done.stderr)
        # A wait cut short by the limit returns its own error; no Luau runs after it to raise "timeout".
        self.assertEqual(done.stdout, "true\tnil\tfifo: timed out\n" * 2 + "true\tone two\ntrue\ttrue\n"
                                      "true\tnil\tfifo: timed out\n\ttrue\n")
        self.assertLess(time.monotonic() - started, 20)
        for _ in range(100):
            if os.path.getsize(os.path.join(self.project, "got.txt")) == 200000:
                break
            time.sleep(0.05)
        self.assertEqual(os.path.getsize(os.path.join(self.project, "got.txt")), 200000)

    def test_a_stop_reaches_a_read_of_a_fifo(self):
        code, out = self.run_interrupted(
            "host.sh('mkfifo fifo; echo $$ > cmd.pid')\nprint(host.try(host.read, 'fifo'))\nprint('after')\n")
        self.assertEqual((code, out), (0, "false\tinterrupted\nafter\n"))

    def test_clip_keeps_both_ends_cut_at_a_line_or_a_character(self):
        script = ("local function show(...) print(select('#', ...), json.encode({...})) end\n"
                  "show(host.clip('abc', 3))\n"
                  "show(host.clip(string.rep('xxxxxxx\\n', 4), 20))\n"  # a line near each cut
                  "show(host.clip('a\\nbbbbbbbbbbbbbbbbbbbbbbbbbbbb\\nc', 12))\n"  # lines too far: cut at a character
                  "show(host.clip(string.rep('\\u{e9}', 10), 7))\n")  # each cut falls inside an é
        done = self.run_hx("--run", "-", stdin=script)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines(), [
            '1\t["abc"]', '2\t["xxxxxxx\\n","xxxxxxx\\n"]', '2\t["a\\nbbbb","bbbb\\nc"]', '2\t["é","é"]'])

    # --- extensions and the project's own loop ---

    def test_the_model_extends_itself_and_the_extension_stays(self):
        module = "-- shouts a word\nreturn { shout = function(w) return w:upper() .. '!' end }\n"
        done = self.run_hx("go", script=[
            call("write", {"path": ".hx/ext/shout.luau", "content": module}, "c1"),
            call("lua", {"code": "return ext('shout').shout('hey')"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results()[1], "HEY!")
        self.assertNotIn("Extensions:", self.model.requests[0]["system"])
        # It is told what runs it, not that a file which may not exist does.
        self.assertIn("the loop built into hx", self.model.requests[0]["system"])
        self.assertNotIn("running you is .hx/loop.luau", self.model.requests[0]["system"])
        # A later session is told about it.
        self.run_hx("again", script=[say("ok")])
        self.assertIn("- shout: shouts a word", self.model.requests[-1]["system"])
        # Each session's conversation was saved, the first one's too, though .hx did not exist when it started.
        logs = [json.loads(read(self.project, ".hx", "log", name))
                for name in os.listdir(os.path.join(self.project, ".hx", "log")) if name.endswith(".json")]
        self.assertEqual(sorted(log["messages"][0]["content"] for log in logs), ["again", "go"])
        saved = next(log for log in logs if log["messages"][0]["content"] == "again")
        self.assertEqual(saved["system"], self.model.requests[-1]["system"])
        # The logs hold file contents, so git does not see them, though nothing but the model made .hx/.
        self.assertEqual(read(self.project, ".hx", "log", ".gitignore"), "*\n")
        git = dict(cwd=self.project, env=self.env(), capture_output=True, text=True, check=True)
        subprocess.run(["git", "init", "-q"], **git)
        subprocess.run(["git", "add", "-A"], **git)
        self.assertEqual(subprocess.run(["git", "ls-files"], **git).stdout, ".hx/ext/shout.luau\n")

    def test_web_and_ocr_go_to_deepseek_with_its_key_alone(self):
        tools = Tools()
        self.addCleanup(tools.server_close)
        self.addCleanup(tools.shutdown)
        with open(os.path.join(self.project, "scan.pdf"), "wb") as f:
            f.write(b"%PDF-1.7 not really\x00\xff")
        with open(os.path.join(self.project, "photo.png"), "wb") as f:
            f.write(b"\x89PNG fake")
        ds_key = "ds-tools-key-42"
        code = ("local web, ocr = ext('web'), ext('ocr') "
                "print(web.search('luau language', {count = 3})) print(web.fetch('https://luau.org')) "
                "print(ocr.read('scan.pdf')) return ocr.read('photo.png')")
        done = self.run_hx("go", HX_TOOLS_URL=tools.url + "/", DEEPSEEK_API_KEY=ds_key, script=[
            call("lua", {"code": code}, "c1"),
            call("bash", {"command": "env"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        result = self.tool_results()[0]
        self.assertIn("1. Luau\n   https://luau.org\n   A small fast language.", result)
        self.assertIn("# Luau\n\nA small fast language.", result)
        self.assertEqual(result.count("Recognized words"), 2)
        # Each call went to its endpoint, with the DeepSeek key as a bearer token.
        self.assertEqual([r["path"] for r in tools.requests],
                         ["/v1/tools/search", "/v1/tools/fetch", "/v1/tools/ocr", "/v1/tools/ocr"])
        for r in tools.requests:
            self.assertEqual(r["headers"]["authorization"], f"Bearer {ds_key}")
            self.assertNotIn("x-api-key", r["headers"])
        self.assertEqual(json.loads(tools.requests[0]["body"]), {"query": "luau language", "count": 3})
        self.assertEqual(json.loads(tools.requests[1]["body"]), {"url": "https://luau.org"})
        # OCR uploads the file as a form, with the purpose the file's kind calls for.
        pdf, png = tools.requests[2], tools.requests[3]
        self.assertTrue(pdf["headers"]["content-type"].startswith("multipart/form-data; boundary="))
        self.assertIn(b'name="file"; filename="scan.pdf"', pdf["body"])
        self.assertIn(b"%PDF-1.7 not really\x00\xff", pdf["body"])
        self.assertIn(b'name="purpose"\r\n\r\nrich_layout_recognition', pdf["body"])
        self.assertIn(b'name="purpose"\r\n\r\ntext_recognition', png["body"])
        # The model's endpoint gets its own key, and neither key reaches the model or the commands.
        self.assertEqual(self.model.headers[0]["x-api-key"], KEY)
        self.assertNotIn(ds_key, json.dumps(self.model.requests))
        self.assertNotIn(ds_key, self.tool_results()[1])
        # The built-in extensions are listed, since the key is there.
        self.assertIn("- web (built in): searches the web", self.model.requests[0]["system"])
        self.assertIn("- ocr (built in): reads the text", self.model.requests[0]["system"])

    def test_exa_deep_search_goes_to_exa_with_its_key_alone(self):
        tools = Tools()
        self.addCleanup(tools.server_close)
        self.addCleanup(tools.shutdown)
        exa_key = "exa-key-77"
        code = ("local r = ext('exa').deep('what is luau', 'prefer luau.org; pull the type system', "
                "{queries = {'luau gradual typing'}}) "
                "assert(r.answer and r.results[1].highlights[2] and r.grounding[1].field == 'content' "
                "and r.cost == 0.012) return r")
        done = self.run_hx("go", HX_EXA_URL=tools.url.replace("/v1/tools", "/exa/search"), EXA_API_KEY=exa_key,
                           script=[call("lua", {"code": code}, "c1"),
                                   call("lua", {"code": "return ext('web').search('x')"}, "c2"),
                                   call("bash", {"command": "env"}, "c3"),
                                   say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        results = self.tool_results()
        self.assertIn("Luau is a fast, small, gradually typed language.\n\nnotice: site: removed.\n\nSources:\n"
                      "1. Luau (2026-08-26)\n   https://luau.org\n"
                      "   Luau is a small fast language. ... It has gradual types.", results[0])
        # One request, to Exa, with its key as x-api-key, asking for deep research with highlights and a text answer.
        self.assertEqual([r["path"] for r in tools.requests], ["/exa/search"])
        request = tools.requests[0]
        self.assertEqual(request["headers"]["x-api-key"], exa_key)
        self.assertNotIn("authorization", request["headers"])
        body = json.loads(request["body"])
        self.assertEqual({k: body[k] for k in ("query", "objective", "additionalQueries", "type", "numResults",
                                               "contents")},
                         {"query": "what is luau", "objective": "prefer luau.org; pull the type system",
                          "additionalQueries": ["luau gradual typing"], "type": "deep", "numResults": 10,
                          "contents": {"highlights": True}})
        self.assertEqual(body["outputSchema"]["type"], "text")
        # The warning is logged for whoever runs hx; the model does not see it.
        self.assertIn("exa warning: x is ignored.", done.stderr)
        self.assertNotIn("x is ignored", json.dumps(self.model.requests))
        # The Exa key does not open DeepSeek's tools, and reaches neither the model nor the commands.
        self.assertIn("without DEEPSEEK_API_KEY", results[1])
        self.assertNotIn(exa_key, json.dumps(self.model.requests))
        self.assertNotIn(exa_key, results[2])
        # Only exa is listed, since only its key is there.
        system = self.model.requests[0]["system"]
        self.assertIn("- exa (built in): researches a question", system)
        self.assertNotIn("- web (built in)", system)

    def test_without_a_deepseek_key_the_tools_say_so(self):
        done = self.run_hx("go", script=[
            call("lua", {"code": "return ext('web').search('luau')"}, "c1"),
            call("lua", {"code": "return host.tool('search', '{}')"}, "c2"),
            call("lua", {"code": "return host.tool('chat', '{}')"}, "c3"),
            call("lua", {"code": "return ext('ocr').read('nope.pdf')"}, "c4"),
            call("lua", {"code": "return ext('exa').deep('luau', 'why')"}, "c5"),
            call("lua", {"code": "return host.tool('exa', '{}')"}, "c6"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        results = self.tool_results()
        self.assertIn("without DEEPSEEK_API_KEY", results[0])
        self.assertIn("no DEEPSEEK_API_KEY", results[1])
        self.assertIn("no tool chat", results[2])
        self.assertIn("without DEEPSEEK_API_KEY", results[3])
        self.assertIn("without EXA_API_KEY", results[4])
        self.assertIn("no EXA_API_KEY", results[5])
        self.assertNotIn("Extensions:", self.model.requests[0]["system"])  # none is listed without its key

    def test_ocr_of_a_file_outside_the_wall_is_refused_before_sending(self):
        tools = Tools()
        self.addCleanup(tools.server_close)
        self.addCleanup(tools.shutdown)
        secret = os.path.join(self.home, "secret.pdf")
        with open(secret, "w") as f:
            f.write("private")
        done = self.run_hx("go", HX_TOOLS_URL=tools.url, DEEPSEEK_API_KEY="k", script=[
            call("lua", {"code": f"return ext('ocr').read({lua_literal(secret)})"}, "c1"),
            call("lua", {"code": "return ext('ocr').read('.')"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("Permission denied", self.tool_results()[0])
        self.assertIn("not a regular file", self.tool_results()[1])
        self.assertEqual(tools.requests, [])

    def test_project_notes_reach_the_system_prompt(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        with open(os.path.join(self.project, ".hx", "prompt.md"), "w") as f:
            f.write("Run the tests with make check.")
        self.run_hx("go", script=[say("ok")])
        self.assertIn("Run the tests with make check.", self.model.requests[0]["system"])

    def test_the_hx_skill_is_built_in_and_a_project_s_own_takes_its_place(self):
        done = self.run_hx("go", script=[call("lua", {"code": "return skill('hx')"}, "c1"), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        # No directory here holds a skill, and the prompt still lists the one hx is built with.
        self.assertIn("\n- hx (built in): Use, extend or change hx", self.model.requests[0]["system"])
        # The paragraph on extending hx points to it: a model that is not told loads nothing and works from that
        # paragraph alone.
        self.assertIn('skill("hx") says which of these a need calls for', self.model.requests[0]["system"])
        # It loads whole: the text after the front matter of the file Claude Code reads in this repository.
        body = read(REPO, ".claude", "skills", "hx", "SKILL.md").split("---\n", 2)[2].strip()
        self.assertEqual(self.tool_results()[0], "[skill hx, built into hx. Every path in it is the project's.]\n\n" + body)
        # A project's own skill of that name takes its place.
        os.makedirs(os.path.join(self.project, ".hx", "skills", "hx"))
        with open(os.path.join(self.project, ".hx", "skills", "hx", "SKILL.md"), "w") as f:
            f.write("---\nname: hx\ndescription: This project's own.\n---\nOurs.\n")
        self.run_hx("again", script=[call("lua", {"code": "return skill('hx')"}, "c2"), say("ok")])
        self.assertIn("\n- hx: This project's own.", self.model.requests[-2]["system"])
        self.assertNotIn("(built in)", self.model.requests[-2]["system"])
        self.assertEqual(self.tool_results()[1], "[skill hx, in .hx/skills/hx/. A path in it that names one of the skill's "
                         "own files is relative to that directory; everything else is the project's.]\n\nOurs.")

    def write_files(self, *paths):
        for path in paths:
            os.makedirs(os.path.dirname(os.path.join(self.project, path)), exist_ok=True)
            with open(os.path.join(self.project, path), "w") as f:
                f.write("x\n")

    def test_the_system_prompt_shows_the_files_git_lists(self):
        git = dict(cwd=self.project, env=self.env(), capture_output=True, text=True, check=True)
        subprocess.run(["git", "init", "-q"], **git)
        self.write_files("src/main.c", "src/util/text.c", "README.md", "new.txt", "build/out.o")
        with open(os.path.join(self.project, ".gitignore"), "w") as f:
            f.write("build/\n")
        subprocess.run(["git", "add", "src", "README.md", ".gitignore"], **git)
        self.run_hx("go", script=[say("ok")])
        system = self.model.requests[0]["system"]
        # Tracked files and a new one that is not ignored, directories first; what .gitignore names is left out.
        self.assertIn("5 in all, as git lists them", system)
        self.assertIn("\nsrc/\n  util/\n    text.c\n  main.c\n.gitignore\nREADME.md\nnew.txt", system)
        self.assertNotIn("build/", system)
        self.assertNotIn("out.o", system)
        # A project that wants no tree, or fewer lines of it, says so in its loop.
        self.run_hx("--init")
        loop = os.path.join(self.project, ".hx", "loop.luau")
        source = read(loop)
        with open(loop, "w") as f:
            f.write(source.replace("-- M.max_replies = 100", "M.layout_lines = 0"))
        done = self.run_hx("go", script=[say("ok")])
        self.assertNotIn("loop.luau", done.stderr)  # the project's loop loaded
        self.assertNotIn("main.c", self.model.requests[-1]["system"])

    def test_without_git_the_system_prompt_shows_the_project_directory(self):
        self.write_files("src/main.c", "notes.txt")
        self.run_hx("go", script=[say("ok")])
        system = self.model.requests[0]["system"]
        self.assertIn("its own entries only, since git lists no files here.\nsrc/\nnotes.txt", system)
        self.assertNotIn("main.c", system)

    def test_init_writes_an_overlay_on_the_built_in_loop(self):
        done = self.run_hx("--init")
        self.assertEqual(done.returncode, 0, done.stderr)
        loop = os.path.join(self.project, ".hx", "loop.luau")
        source = read(loop)
        self.assertEqual(source, read(REPO, "lua", "overlay.luau"))
        self.assertIn("host.load(host.default_loop", source)
        self.assertEqual(self.run_hx("--init").returncode, 2)  # never overwrites
        # Unchanged, it behaves as the built-in loop does.
        done = self.run_hx("go", script=[say("ok")])
        self.assertEqual((done.returncode, done.stdout), (0, "ok\n"), done.stderr)
        # A setting changed in it, and a function replaced in it, change what is sent.
        with open(loop, "w") as f:
            f.write(source.replace('-- M.max_replies = 100',
                                   'M.model = "project-model"')
                    .replace("-- local built_in_prompt", "local built_in_prompt")
                    .replace("-- function M.system_prompt()", "function M.system_prompt()")
                    .replace("-- \treturn built_in_prompt()", "\treturn built_in_prompt()")
                    .replace("-- end", "end"))
        self.run_hx("go", script=[say("ok")])
        sent = self.model.requests[-1]
        self.assertEqual(sent["model"], "project-model")
        self.assertTrue(sent["system"].startswith("You are a coding agent"))
        self.assertTrue(sent["system"].endswith("Run `make check` before you report."))

    def test_init_full_copies_the_whole_loop(self):
        done = self.run_hx("--init", "--full")
        self.assertEqual(done.returncode, 0, done.stderr)
        loop = os.path.join(self.project, ".hx", "loop.luau")
        source = read(loop)
        self.assertEqual(source, read(REPO, "lua", "loop.luau"))
        # The copy is what runs: change it and the behaviour changes.
        with open(loop, "w") as f:
            f.write(source.replace('M.model = host.model',
                                   'M.model = "project-model"'))
        self.run_hx("go", script=[say("ok")])
        self.assertEqual(self.model.requests[0]["model"], "project-model")

    def test_a_broken_project_loop_falls_back_to_the_built_in_one(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        with open(os.path.join(self.project, ".hx", "loop.luau"), "w") as f:
            f.write("local M = {\n")
        done = self.run_hx("go", script=[say("still works")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "still works\n")
        self.assertIn("does not compile", done.stderr)
        self.assertIn("using the built-in loop", done.stderr)

    def test_a_project_loop_that_raises_while_loading_falls_back(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        with open(os.path.join(self.project, ".hx", "loop.luau"), "w") as f:
            f.write("local M = host.load(host.default_loop)()\nM.tools.nope.run = nil\nreturn M\n")
        done = self.run_hx("go", script=[say("still works")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "still works\n")
        self.assertIn("raised an error while loading (.hx/loop.luau:2:", done.stderr)
        self.assertIn("using the built-in loop", done.stderr)

    def test_a_project_loop_that_is_not_a_module_falls_back(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        for source in ["return {}\n", "local M = host.load(host.default_loop)()\nM.main = nil\nreturn M\n", "print('x')\n"]:
            with open(os.path.join(self.project, ".hx", "loop.luau"), "w") as f:
                f.write(source)
            done = self.run_hx("go", script=[say("still works")])
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertTrue(done.stdout.endswith("still works\n"), source)
            self.assertIn("does not return a table with a main function; using the built-in loop", done.stderr)

    def test_an_error_inside_the_project_loop_is_not_a_fallback(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        with open(os.path.join(self.project, ".hx", "loop.luau"), "w") as f:
            f.write("return { main = function() error('main broke', 0) end }\n")
        done = self.run_hx("go", script=[say("never")])
        self.assertEqual(done.returncode, 1)
        self.assertIn("hx: main broke", done.stderr)
        self.assertNotIn("built-in", done.stderr)
        self.assertEqual(self.model.requests, [])

    def test_a_wrong_argument_name_is_named_with_the_right_ones(self):
        with open(os.path.join(self.project, "f.txt"), "w") as f:
            f.write("one two\n")
        done = self.run_hx("go", script=[
            call("lua", {"command": "return 1"}, "c1"),
            call("lua", {"code": "return edit('f.txt', 'one')"}, "c2"),
            call("lua", {"code": "return edit('f.txt', 'one', '1')"}, "c3"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results(), [
            'error: unknown argument "command": lua takes code',
            "error: edit: argument 3 (new) must be a string, not nil; usage: edit(path, old, new, all?)",
            "replaced 1 occurrence in f.txt"])
        self.assertEqual(read(self.project, "f.txt"), "1 two\n")

    def test_one_call_can_do_several_things_and_a_failure_keeps_what_came_before(self):
        with open(os.path.join(self.project, "f.txt"), "w") as f:
            f.write("a\nb\n")
        batch = ("for _, name in {'x', 'y'} do write(name .. '.txt', name) end "
                 "print(edit('f.txt', 'a', 'A')) print(edit('f.txt', 'zzz', 'Z')) print('never')")
        done = self.run_hx("go", script=[call("lua", {"code": batch}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results(), [
            "replaced 1 occurrence in f.txt\n"
            "error: edit: old text not found in f.txt: read the file again and copy the text exactly"])
        self.assertIs(self.model.requests[1]["messages"][-1]["content"][0]["is_error"], True)
        self.assertEqual((read(self.project, "x.txt"), read(self.project, "y.txt")), ("x", "y"))
        self.assertEqual(read(self.project, "f.txt"), "A\nb\n")
        self.assertIn("  write x.txt\n  write y.txt\n  edit f.txt\n  edit f.txt\n", done.stderr)

    def test_a_long_output_is_kept_whole_in_a_file_with_or_without_the_sandbox(self):
        script = ("local loop = host.load(host.default_loop, '=loop')() loop.output_limit = 100 "
                  "local out = loop.tools[1].run({ code = 'return bash(\\'seq 1 5000\\')' }) "
                  "local path = out:match('the whole output is in (%S+)') "
                  "print(path ~= nil and path:sub(1, #host.env('TMPDIR')) == host.env('TMPDIR'), "
                  "path and host.read(path) == host.sh('seq 1 5000').out .. '[exit 0]')")
        for flags in ([], ["--no-sandbox"]):
            done = self.run_hx(*flags, "--run", "-", stdin=script)
            self.assertEqual((done.returncode, done.stdout), (0, "true\ttrue\n"), (flags, done.stderr))

    def test_a_loop_without_tools_sends_an_empty_list(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        with open(os.path.join(self.project, ".hx", "loop.luau"), "w") as f:
            f.write("local M = assert(host.load(host.default_loop, '=loop'))() M.tools = {} return M")
        done = self.run_hx("go", script=[say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[0]["tools"], [])

    def test_control_characters_reach_a_terminal_only_as_text(self):
        words = "\x1b]52;c;aGk=\x07red\x1b[31m!"
        # The model's own code writes to the terminal through host.log and host.input, and prints through _G.print.
        writes = ('host.log("log\\27]0;pwned\\7") _G.print("print\\27]0;pwned\\7") '
                  'return host.input("ask\\27]0;pwned\\7")')
        script = [call("bash", {"command": "true # \x1b]0;title\x07"}, "c1"), call("lua", {"code": writes}, "c2"),
                  say(words)]
        # Into a pipe, the model's words are passed on unchanged; the progress line never carries the escapes, nor
        # does what the model's code writes there.
        done = self.run_hx("go", script=script, stdin="typed\n")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, words + "\n")
        self.assertIn("bash true # ]0;title", done.stderr)
        self.assertIn("log]0;pwned", done.stderr)
        self.assertIn("ask]0;pwned", done.stderr)
        self.assertNotIn("\x1b", done.stderr)
        # _G.print is the print that goes to the model, which reads the text as it is; no terminal sees it.
        self.assertEqual(self.tool_results()[1], "print\x1b]0;pwned\x07\ntyped")
        # On a terminal they are stripped.
        self.model.script = list(script)
        master, slave = pty.openpty()
        process = subprocess.Popen([HX, "go"], cwd=self.project, env=self.env(), stdin=subprocess.DEVNULL, stdout=slave,
                                   stderr=subprocess.DEVNULL)
        os.close(slave)
        out = b""
        try:
            while True:
                try:
                    chunk = os.read(master, 4096)
                except OSError:  # EIO once the child has closed the terminal
                    break
                if not chunk:
                    break
                out += chunk
            self.assertEqual(process.wait(timeout=20), 0)
        finally:
            process.kill()
            os.close(master)
        self.assertEqual(out, b"]52;c;aGk=red[31m!\r\n")

    def test_a_session_that_makes_hx_keeps_its_log(self):
        # No .hx when the session starts: the model makes it by writing its first extension.
        module = "-- says hi\nreturn { hi = function() return 'hi' end }\n"
        done = self.run_hx("go", script=[call("write", {"path": ".hx/ext/hi.luau", "content": module}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        logs = [name for name in os.listdir(os.path.join(self.project, ".hx", "log")) if name.endswith(".json")]
        self.assertEqual(len(logs), 1)
        self.assertEqual(len(json.loads(read(self.project, ".hx", "log", logs[0]))["messages"]), 4)

    def test_run_executes_a_script(self):
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("print(json.encode(host.args), host.sh('echo hi').out)\n")
        done = self.run_hx("--run", "s.luau", "a", "--b")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, '["a","--b"]\thi\n\n')

    def test_a_task_may_start_with_any_word(self):
        done = self.run_hx("run", "the", "tests", script=[say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[0]["messages"][0]["content"], "run the tests")
        done = self.run_hx("init", script=[say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[-1]["messages"][0]["content"], "init")
        self.assertFalse(os.path.exists(os.path.join(self.project, ".hx", "loop.luau")))

    def test_a_script_error_is_reported(self):
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("error('it broke')\n")
        done = self.run_hx("--run", "s.luau")
        self.assertEqual(done.returncode, 1)
        self.assertIn("s.luau:1: it broke", done.stderr)


    def test_a_narrower_wall_holds_its_command_and_what_it_leaves(self):
        with open(os.path.join(self.project, "secret.txt"), "w") as f:
            f.write("s3cret\n")
        with open(os.path.join(self.project, "wall.luau"), "w") as f:
            f.write("""
local box = host.env("TMPDIR") .. "/box"
host.sh("mkdir -p " .. box)
host.sh("sleep 61 > /dev/null 2>&1 &")  -- an ordinary background job
print(host.sh("cat secret.txt", { write = { box } }).code ~= 0)
print(host.sh("echo inside > " .. box .. "/f && cat " .. box .. "/f", { write = { box } }).out)
host.sh("setsid sleep 62 > /dev/null 2>&1 & echo left", { write = { box } })
print(host.sh("pgrep -x sleep -a").out)
""")
        done = self.run_hx("--run", "wall.luau")
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        self.assertEqual(lines[:2], ["true", "inside"])
        self.assertIn("sleep 61", done.stdout)
        self.assertNotIn("sleep 62", done.stdout)

    # --- the judge ---

    def make_kit(self, where=None, run="#!/bin/sh\nsh prog.sh\n",
                 check='#!/bin/sh\ngrep -qx 42 "$1/.judge/stdout" || { echo "wrong: $(head -c 80 "$1/.judge/stdout")"; exit 1; }\n',
                 config='{"runs": 3}', prog="sleep 0.3\necho 42\n"):
        """A kit that times prog.sh and passes only when it prints 42."""
        kit = where or os.path.join(self.root, "kit")
        os.makedirs(kit, exist_ok=True)
        for name, body in [("run", run), ("check", check)]:
            if body is not None:
                with open(os.path.join(kit, name), "w") as f:
                    f.write(body)
                os.chmod(os.path.join(kit, name), 0o755)
        with open(os.path.join(kit, "kit.json"), "w") as f:
            f.write(config)
        with open(os.path.join(self.project, "prog.sh"), "w") as f:
            f.write(prog)
        return kit

    def judge_state(self):
        base = os.path.join(self.home, ".local", "state", "hx", "judge")
        (name,) = os.listdir(base)
        return os.path.join(base, name)

    def ledger(self):
        return [json.loads(line) for line in read(self.judge_state(), "ledger.jsonl").splitlines()]

    def drive(self, kit, body, *flags, timeout=120):
        """Runs a script that submits to the judge; submit(note) returns the verdict, or the pending count."""
        with open(os.path.join(self.project, "drive.luau"), "w") as f:
            f.write("""
function submit(note)
    local lines, pending = host.judge(json.encode({ note = note }))
    return lines[1] and json.decode(lines[1]) or pending
end
function say(text) print((tostring(text):gsub("\\n", " | "))) end
""" + body)
        return subprocess.run([HX, *flags, "--judge", kit, "--run", "drive.luau"], cwd=self.project,
                              env=self.env(XDG_STATE_HOME=""), capture_output=True, text=True, timeout=timeout)

    def test_judge_keeps_only_confirmed_wins(self):
        kit = self.make_kit()
        done = self.drive(kit, f"""
local function try(note, prog) host.write("prog.sh", prog); say(submit(note)) end
try("same", "sleep 0.3\\necho 42\\n")
try("faster", "sleep 0.05\\necho 42\\n")
try("wrong", "echo 41\\n")
try("peek", "cat {kit}/check >/dev/null 2>&1 && echo leaked || echo 42\\n")
try("slower", "sleep 0.2\\necho 42\\n")
""")
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        self.assertTrue(lines[0].startswith("Trial 1. no different:"), lines[0])
        self.assertIn("The champion is still the starting point.", lines[0])
        self.assertRegex(lines[1], r"^Trial 2\. win: \+\d+\.\d% against the champion \(medians 0\.\d+ s and 0\.\d+ s\)")
        self.assertTrue(lines[2].startswith("Trial 3. invalid: the check failed: | wrong: 41"), lines[2])
        # The candidate's run cannot read the kit: had it, it would have printed "leaked" and failed the check.
        self.assertTrue(lines[3].startswith("Trial 4. win:"), lines[3])
        self.assertTrue(lines[4].startswith("Trial 5. worse:"), lines[4])
        self.assertIn("hx judge: 5 trials: 2 win, 1 no different, 1 worse, 1 invalid", done.stderr)
        self.assertRegex(done.stderr, r"the champion is trial 4 \(0\.\d+ s, the start 0\.3\d+ s\)")
        ledger = self.ledger()
        self.assertEqual([e["verdict"] for e in ledger], ["start", "no different", "win", "invalid", "win", "worse"])
        self.assertEqual(ledger[2]["note"], "faster")
        self.assertEqual(len(ledger[2]["screen"]["mine"]), 3)
        state = self.judge_state()
        self.assertEqual(sorted(os.listdir(state)), ["champion.patch", "ledger.jsonl", "trials"])
        self.assertEqual(sorted(os.listdir(os.path.join(state, "trials"))), [f"{n}.patch" for n in range(1, 6)])

    def test_judge_the_start_is_measured_before_the_agent_begins(self):
        kit = self.make_kit()
        done = self.drive(kit, 'host.write("prog.sh", "sleep 0.05\\necho 42\\n")\nsay(submit("fast"))\n')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertRegex(done.stderr, r"the starting point passes \(0\.3\d+ s\)")
        self.assertTrue(done.stdout.startswith("Trial 1. win:"), done.stdout)

    def test_judge_a_patch_carries_every_kind_of_change(self):
        kit = self.make_kit(check=None, config='{"runs": 1}')
        os.makedirs(os.path.join(self.project, "a dir"))
        with open(os.path.join(self.project, "gone.txt"), "w") as f:
            f.write("bye\n")
        original = os.path.join(self.root, "original")
        shutil.copytree(self.project, original, symlinks=True)
        done = self.drive(kit, """
host.write("fast one.sh", "echo 'quoted'\\n")
host.sh("chmod +x 'fast one.sh' && printf '\\\\000\\\\001\\\\377' > data.bin && ln -s prog.sh link && rm gone.txt")
host.write("a dir/it's", "x\\n")
say(submit("all kinds"))
""")
        self.assertEqual(done.returncode, 0, done.stderr)
        applied = subprocess.run(["git", "apply", os.path.join(self.judge_state(), "trials", "1.patch")], cwd=original,
                                 capture_output=True, text=True)
        self.assertEqual(applied.returncode, 0, applied.stderr)
        os.remove(os.path.join(self.project, "drive.luau"))
        def content(root, name):
            with open(os.path.join(root, name), "rb") as f:
                return f.read()
        for name in ["fast one.sh", "data.bin", "a dir/it's", "prog.sh"]:
            self.assertEqual(content(original, name), content(self.project, name), name)
        self.assertTrue(os.access(os.path.join(original, "fast one.sh"), os.X_OK))
        self.assertEqual(os.readlink(os.path.join(original, "link")), "prog.sh")
        self.assertFalse(os.path.exists(os.path.join(original, "gone.txt")))

    def test_judge_a_score_must_be_positive_and_finite(self):
        kit = self.make_kit(check='#!/bin/sh\ncat "$1/.judge/stdout"\n', config='{"runs": 1}', prog="echo 2\n")
        done = self.drive(kit, """
for _, score in { "inf", "nan", "0", "-3", "3" } do
    host.write("prog.sh", "echo " .. score .. "\\n")
    say(submit(score))
end
""")
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        for line in lines[:4]:
            self.assertIn("invalid: the check's score must be a positive, finite number", line)
        self.assertTrue(lines[4].startswith("Trial 5. win: +50.0% against the champion (medians score 3 and score 2)"), lines[4])
        self.assertEqual([e["verdict"] for e in self.ledger()], ["start", "invalid", "invalid", "invalid", "invalid", "win"])

    def test_judge_a_verdict_cut_short_comes_with_the_next_call(self):
        kit = self.make_kit(config='{"runs": 1}', prog="sleep 1\necho 42\n")
        done = self.drive(kit, """
host.write("prog.sh", "sleep 0.1\\necho 42\\n")
local ok, lines, pending = host.timed(3, host.judge, json.encode({ note = "slow kit" }))
print(ok, #lines, pending)
local later, still = host.judge()
print(#later, still, json.decode(later[1]):match("^Trial 1%. %w+"))
""")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.splitlines(), ["true\t0\t1", "1\t0\tTrial 1. win"])

    def test_judge_a_kit_it_cannot_use_stops_hx_before_the_agent(self):
        for config, message in [('{"run": 1}', "kit.json: unknown key run"), ('{"runs": "x"}', "runs must be a number"),
                                ("[", "kit.json is not a JSON object")]:
            kit = self.make_kit(config=config)
            done = self.run_hx("--judge", kit, "go", XDG_STATE_HOME="")
            self.assertEqual(done.returncode, 2, done.stderr)
            self.assertIn(message, done.stderr)
        kit = self.make_kit(config="{}")
        with open(os.path.join(kit, "build"), "w") as f:
            f.write("#!/bin/sh\n")
        done = self.run_hx("--judge", kit, "go")
        self.assertEqual(done.returncode, 2)
        self.assertIn("build must be an executable file", done.stderr)
        os.remove(os.path.join(kit, "build"))
        os.remove(os.path.join(kit, "check"))
        os.makedirs(os.path.join(kit, "check"))
        done = self.run_hx("--judge", kit, "go")
        self.assertEqual(done.returncode, 2)
        self.assertIn("check must be an executable file", done.stderr)
        self.assertEqual(self.model.requests, [])

    def test_judge_a_failing_start_makes_the_first_passing_submission_champion(self):
        kit = self.make_kit(prog="echo 41\n")
        done = self.drive(kit, 'host.write("prog.sh", "echo 40\\n"); say(submit("still wrong"))\n'
                               'host.write("prog.sh", "echo 42\\n"); say(submit("fixed"))\n')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("the starting point does not pass", done.stderr)
        lines = done.stdout.splitlines()
        self.assertTrue(lines[0].startswith("Trial 1. invalid: nothing passes yet."), lines[0])
        self.assertTrue(lines[1].startswith("Trial 2. champion:"), lines[1])
        self.assertEqual([e["verdict"] for e in self.ledger()], ["start fails", "invalid", "champion"])
        self.assertIn("the champion is trial 2 (", done.stderr)

    def test_judge_a_trial_it_cannot_finish_is_recorded_as_an_error(self):
        flags = os.path.join(self.root, "flags")
        os.makedirs(flags)
        # Once the flag is up, the champion's code (which sleeps 0.3) fails to run; the candidate's does not.
        kit = self.make_kit(run=f"#!/bin/sh\ntest -e {flags}/on && grep -q 0.3 prog.sh && exit 3\nsh prog.sh\n")
        done = self.drive(kit, f'''
host.write("prog.sh", "sleep 0.01\\necho 42\\n")
host.write("{flags}/on", "")
say(submit("fine"))
''', "--rw", flags)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue(done.stdout.startswith("Trial 1. error: the judge could not finish it"), done.stdout)
        self.assertIn("the champion failed", done.stdout)
        self.assertEqual([e["verdict"] for e in self.ledger()], ["start", "error"])
        self.assertIn("1 error", done.stderr)
        self.assertEqual(os.listdir(os.path.join(self.judge_state(), "trials")), ["1.patch"])

    def test_judge_a_stop_during_a_trial_ends_it_and_the_summary_follows(self):
        kit = self.make_kit(config='{"runs": 1}', prog="sleep 0.1\necho 42\n")
        with open(os.path.join(self.project, "drive.luau"), "w") as f:
            f.write('host.write("prog.sh", "sleep 30\\necho 42\\n")\nprint(host.judge(json.encode({ note = "slow" })))\n')
        proc = subprocess.Popen([HX, "--judge", kit, "--run", "drive.luau"], cwd=self.project,
                                env=self.env(XDG_STATE_HOME=""), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        time.sleep(3)
        started = time.monotonic()
        proc.send_signal(signal.SIGINT)
        out, err = proc.communicate(timeout=30)
        self.assertLess(time.monotonic() - started, 5)
        self.assertEqual(proc.returncode, 130, err)
        self.assertIn("hx judge: 1 trial: 1 stopped", err)
        self.assertEqual([e["verdict"] for e in self.ledger()], ["start", "stopped"])
        time.sleep(0.5)
        left = subprocess.run(["pgrep", "-f", kit], capture_output=True, text=True).stdout
        self.assertEqual(left, "", "the judge or its run outlived hx")

    def test_judge_a_check_does_not_follow_links_the_candidate_left(self):
        kit = self.make_kit(config='{"runs": 1}', prog="echo 42\n")
        with open(os.path.join(kit, "expected"), "w") as f:
            f.write("42\n")
        with open(os.path.join(kit, "check"), "w") as f:
            f.write(f'#!/bin/sh\ncmp -s "$1/.judge/stdout" {kit}/expected || {{ echo wrong; exit 1; }}\n')
        done = self.drive(kit, f"""
host.write("prog.sh", "rm -f .judge/stdout; ln -s {kit}/expected .judge/stdout\\n")
say(submit("a link into the kit, and no work"))
""")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue(done.stdout.startswith("Trial 1. invalid: the check failed"), done.stdout)
        self.assertEqual(read(self.judge_state(), "champion.patch"), "")  # the start is still the champion

    def test_judge_blind_withholds_the_checks_words_too(self):
        kit = self.make_kit(config='{"runs": 1, "blind": true}',
                            check='#!/bin/sh\ngrep -qx 42 "$1/.judge/stdout" || { echo "SECRET expected 42"; exit 1; }\n')
        done = self.drive(kit, 'host.write("prog.sh", "echo 41\\n"); say(submit("wrong"))\n')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.strip(), "Trial 1. invalid: the check failed | The champion is still the starting point.")

    def test_judge_takes_one_submission_at_a_time(self):
        kit = self.make_kit(config='{"runs": 1}', prog="sleep 1\necho 42\n")
        done = self.drive(kit, """
host.write("prog.sh", "sleep 0.5\\necho 42\\n")
local _, lines, pending = host.timed(2.2, host.judge, json.encode({ note = "A" }))
print(#lines, pending)
print(host.try(host.judge, json.encode({ note = "B" })))
local later = host.judge()
print((json.decode(later[1]):match("^Trial 1%. %w+")))
""")
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        self.assertEqual(lines[0], "0\t1")
        self.assertIn("still judging the last submission", lines[1])
        self.assertEqual(lines[2], "Trial 1. win")
        self.assertEqual(len(self.ledger()), 2)

    def test_judge_builds_and_runs_at_one_path(self):
        kit = self.make_kit(config='{"runs": 1}', run="#!/bin/sh\nsh made.sh\n")
        with open(os.path.join(kit, "build"), "w") as f:
            f.write('#!/bin/sh\nprintf "cd %s && sh prog.sh\\n" "$PWD" > made.sh\n')  # records where it was built
        os.chmod(os.path.join(kit, "build"), 0o755)
        done = self.drive(kit, 'host.write("prog.sh", "sleep 0.05\\necho 42\\n"); say(submit("fast"))\n')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("the starting point passes", done.stderr)
        self.assertTrue(done.stdout.startswith("Trial 1. win:"), done.stdout)

    def test_judge_the_agent_cannot_reach_the_kit_or_the_ledger(self):
        kit = self.make_kit()
        state_base = os.path.join(self.home, ".local", "state")
        done = self.drive(kit, f"print(host.read('{kit}/check'), host.list('{state_base}'), host.sh('ls {kit}').code ~= 0)\n")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout.split()[0], "nil")
        self.assertIn("\tnil\t", done.stdout)
        self.assertTrue(done.stdout.rstrip().endswith("true"), done.stdout)

    def test_judge_blind_verdicts_carry_no_numbers_or_output(self):
        kit = self.make_kit(check='#!/bin/sh\ntail -n 1 "$1/.judge/stdout"\n', config='{"runs": 1, "blind": true}',
                            prog="cat built.txt\necho 2\n")
        with open(os.path.join(kit, "build"), "w") as f:
            f.write("#!/bin/sh\necho built > built.txt\n")
        os.chmod(os.path.join(kit, "build"), 0o755)
        done = self.drive(kit, """
host.write("prog.sh", "echo 3\\n"); say(submit("better"))
host.write("prog.sh", "echo SECRET-OUTPUT; exit 1\\n"); say(submit("crash"))
""")
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        self.assertEqual(lines[0], "Trial 1. win: better than the champion, and again on fresh runs. This is the champion now.")
        self.assertEqual(lines[1], "Trial 2. invalid: run exited 1 | The champion is still trial 1.")

    def test_judge_a_kit_the_agent_could_read_is_refused(self):
        kit = self.make_kit(os.path.join(self.project, "kit"))
        done = self.run_hx("--judge", kit, "go")
        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn("the agent could read the kit there", done.stderr)
        done = self.run_hx("--judge", self.make_kit(), "--no-sandbox", "go")
        self.assertEqual(done.returncode, 2)
        self.assertIn("--judge needs the wall", done.stderr)
        link = os.path.join(self.root, "state-link")
        os.symlink(self.project, link)
        done = self.run_hx("--judge", self.make_kit(), "go", XDG_STATE_HOME=link)
        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn("would be within the agent's reach", done.stderr)
        self.assertEqual(self.model.requests, [])

    def test_judge_submit_hands_the_project_over(self):
        kit = self.make_kit()
        done = self.run_hx("--judge", kit, "speed it up", XDG_STATE_HOME="", script=[
            call("bash", {"command": "printf 'sleep 0.05\\necho 42\\n' > prog.sh"}),
            call("submit", {"note": "dropped the long sleep"}, call_id="call_2"), say("It won.")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("This session is judged.", self.model.requests[0]["system"])
        self.assertIn("submit(note?)", self.model.requests[0]["tools"][0]["description"])
        self.assertTrue(self.tool_results()[1].startswith("Trial 1. win:"), self.tool_results())
        self.assertIn("hx judge: 1 trial: 1 win", done.stderr)
        self.assertEqual(self.ledger()[1]["note"], "dropped the long sleep")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    HX = os.path.abspath(sys.argv.pop(1))
    unittest.main(verbosity=1)
