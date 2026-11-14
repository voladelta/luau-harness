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


def read(*path):
    with open(os.path.join(*path)) as f:
        return f.read()


def reply(content, stop="end_turn", input_tokens=10):
    usage = {"input_tokens": input_tokens, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "output_tokens": 5}
    return 200, {"id": "msg_1", "type": "message", "role": "assistant", "content": content, "stop_reason": stop,
                 "stop_sequence": None, "usage": usage}


def text(words):
    return {"type": "text", "text": words}


def use(name, args, call_id="call_1"):
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
        env = {k: v for k, v in os.environ.items() if not k.lower().endswith("_proxy")}
        env.update(HOME=self.home, HX_API_URL=self.model.url, HX_KEY_ENV="HX_TEST_KEY", HX_TEST_KEY=KEY,
                   NO_PROXY="127.0.0.1")
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
        self.assertEqual([t["name"] for t in first["tools"]], ["bash", "read", "write", "edit", "lua"])
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
        done = self.run_hx("go", script=[call("bash", "not an object", "c1"), call("nope", {}, "c2"), call("read", {}, "c3"),
                                         say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        results = self.tool_results()
        self.assertIn("not an object", results[0])
        self.assertIn("no tool named nope", results[1])
        self.assertIn('missing argument "path": read takes path, limit, offset', results[2])
        failed = self.model.requests[1]["messages"][-1]["content"][0]
        self.assertIs(failed["is_error"], True)

    def test_the_task_can_come_from_stdin(self):
        done = self.run_hx(stdin="line one\nline two\n", script=[say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[0]["messages"][0]["content"], "line one\nline two")

    def test_no_task_is_an_error(self):
        done = self.run_hx(stdin="")
        self.assertEqual(done.returncode, 1)
        self.assertIn("no task", done.stderr)

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

    def test_the_home_directory_is_not_a_project(self):
        done = subprocess.run([HX, "go"], cwd=self.home, env=self.env(), capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 2)
        self.assertIn("refusing to treat", done.stderr)
        self.assertEqual(self.model.requests, [])

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

    def test_keep_env_keeps_a_variable(self):
        self.run_hx("--keep-env", "KEPT_TOKEN", "go", KEPT_TOKEN="kept-value", script=[
            call("bash", {"command": "echo $KEPT_TOKEN"}), say("ok")])
        self.assertEqual(self.tool_results(), ["kept-value\n[exit 0]"])

    # --- processes ---

    def test_a_command_that_runs_too_long_is_killed_with_its_children(self):
        started = time.monotonic()
        self.run_hx("go", script=[
            call("bash", {"command": "(sleep 300; echo late) & echo $! > child.pid; sleep 300", "timeout": 1}), say("ok")])
        self.assertLess(time.monotonic() - started, 20)
        self.assertIn("[timed out and was killed]", self.tool_results()[0])
        self.assert_gone(int(read(self.project, "child.pid")))

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

    # --- extensions and the project's own loop ---

    def test_the_model_extends_itself_and_the_extension_stays(self):
        module = "-- shouts a word\nreturn { shout = function(w) return w:upper() .. '!' end }\n"
        done = self.run_hx("go", script=[
            call("write", {"path": ".hx/ext/shout.luau", "content": module}, "c1"),
            call("lua", {"code": "return ext('shout').shout('hey')"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results()[1], "HEY!")
        self.assertNotIn("Extensions in this project", self.model.requests[0]["system"])
        # It is told what runs it, not that a file which may not exist does.
        self.assertIn("the loop built into hx", self.model.requests[0]["system"])
        self.assertNotIn("running you is .hx/loop.luau", self.model.requests[0]["system"])
        # A later session is told about it.
        self.run_hx("again", script=[say("ok")])
        self.assertIn("- shout: shouts a word", self.model.requests[-1]["system"])
        # With a .hx directory, the conversation was saved.
        logs = sorted(name for name in os.listdir(os.path.join(self.project, ".hx", "log")) if name.endswith(".json"))
        self.assertTrue(logs)
        saved = json.loads(read(self.project, ".hx", "log", logs[0]))
        self.assertEqual(saved["messages"][0]["role"], "user")
        self.assertEqual(saved["system"], self.model.requests[-1]["system"])
        # The logs hold file contents, so git does not see them, though nothing but the model made .hx/.
        self.assertEqual(read(self.project, ".hx", "log", ".gitignore"), "*\n")
        git = dict(cwd=self.project, env=self.env(), capture_output=True, text=True, check=True)
        subprocess.run(["git", "init", "-q"], **git)
        subprocess.run(["git", "add", "-A"], **git)
        self.assertEqual(subprocess.run(["git", "ls-files"], **git).stdout, ".hx/ext/shout.luau\n")

    def test_project_notes_reach_the_system_prompt(self):
        os.makedirs(os.path.join(self.project, ".hx"))
        with open(os.path.join(self.project, ".hx", "prompt.md"), "w") as f:
            f.write("Run the tests with make check.")
        self.run_hx("go", script=[say("ok")])
        self.assertIn("Run the tests with make check.", self.model.requests[0]["system"])

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
            f.write(source.replace('-- M.model = "deepseek-v4-pro"', 'M.model = "project-model"')
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
            f.write(source.replace('M.model = host.env("HX_MODEL") or "deepseek-flash"', 'M.model = "project-model"'))
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
            call("edit", {"path": "f.txt", "old": "one", "new_string": "1"}, "c1"),
            call("edit", {"path": "f.txt", "old_string": "one"}, "c2"),
            call("edit", {"path": "f.txt", "old_string": "one", "new_string": "1"}, "c3"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results(), [
            'error: unknown argument "old": edit takes path, old_string, new_string, replace_all',
            'error: missing argument "new_string": edit takes path, old_string, new_string, replace_all',
            "replaced 1 occurrence in f.txt"])
        self.assertEqual(read(self.project, "f.txt"), "1 two\n")

    def test_control_characters_reach_a_terminal_only_as_text(self):
        words = "\x1b]52;c;aGk=\x07red\x1b[31m!"
        script = [call("bash", {"command": "true # \x1b]0;title\x07"}), say(words)]
        # Into a pipe, the model's words are passed on unchanged; the progress line never carries the escapes.
        done = self.run_hx("go", script=script)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, words + "\n")
        self.assertIn("bash true # ]0;title", done.stderr)
        self.assertNotIn("\x1b", done.stderr)
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


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    HX = os.path.abspath(sys.argv.pop(1))
    unittest.main(verbosity=1)
