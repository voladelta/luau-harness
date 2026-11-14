#!/usr/bin/env python3
"""End-to-end checks of hxm against a scripted chat-completions server.

usage: agent_test.py PATH_TO_HXM [unittest arguments]

Each test gets a fresh project directory and a fresh fake home, and scripts the model's replies. The sandbox
checks need a kernel with Landlock.
"""

import http.server
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HXM = None
KEY = "sk-test-0123456789"


class Model(http.server.ThreadingHTTPServer):
    """Answers each request with the next scripted reply and records what was asked."""

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.script = []
        self.requests = []
        self.headers = []
        threading.Thread(target=self.serve_forever, daemon=True).start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_address[1]}/chat/completions"


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        self.server.requests.append(json.loads(body))
        self.server.headers.append(dict(self.headers))
        status, reply = self.server.script.pop(0) if self.server.script else (500, {"error": "script ran out"})
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


def reply(message, finish="stop", prompt_tokens=10):
    usage = {"prompt_tokens": prompt_tokens, "completion_tokens": 5}
    return 200, {"choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage}


def say(text, **kw):
    return reply({"role": "assistant", "content": text}, **kw)


def call(name, args, call_id="call_1", **kw):
    fn = {"name": name, "arguments": json.dumps(args)}
    message = {"role": "assistant", "content": None, "tool_calls": [{"id": call_id, "type": "function", "function": fn}]}
    return reply(message, finish="tool_calls", **kw)


class AgentTest(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="hxm-test-")
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
        env.update(HOME=self.home, HXM_API_URL=self.model.url, HXM_KEY_ENV="HXM_TEST_KEY", HXM_TEST_KEY=KEY,
                   NO_PROXY="127.0.0.1")
        env.update(extra)
        return env

    def run_hxm(self, *args, script=(), stdin=None, **env):
        self.model.script = list(script)
        return subprocess.run([HXM, *args], cwd=self.project, env=self.env(**env), input=stdin,
                              capture_output=True, text=True, timeout=60)

    def tool_results(self):
        """The content of every tool message the model was sent, in order, without repeats."""
        seen, out = set(), []
        for request in self.model.requests:
            for message in request["messages"]:
                if message["role"] == "tool" and message["tool_call_id"] not in seen:
                    seen.add(message["tool_call_id"])
                    out.append(message["content"])
        return out

    # --- the loop ---

    def test_a_tool_call_runs_and_its_result_goes_back(self):
        done = self.run_hxm("make a file", script=[call("bash", {"command": "echo hi > out.txt; echo made"}), say("All done.")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "All done.\n")
        self.assertEqual(read(self.project, "out.txt"), "hi\n")
        first, second = self.model.requests
        self.assertEqual([m["role"] for m in first["messages"]], ["system", "user"])
        self.assertEqual(first["messages"][1]["content"], "make a file")
        self.assertEqual([t["function"]["name"] for t in first["tools"]], ["bash", "read", "write", "edit", "lua"])
        self.assertEqual([m["role"] for m in second["messages"]], ["system", "user", "assistant", "tool"])
        self.assertEqual(second["messages"][3], {"role": "tool", "tool_call_id": "call_1", "content": "made\n[exit 0]"})
        # The earlier messages are resent byte for byte, which is what prompt caching needs.
        self.assertEqual(second["messages"][:2], first["messages"])
        self.assertEqual(second["tools"], first["tools"])

    def test_several_calls_in_one_reply_are_all_answered(self):
        calls = [{"id": f"c{i}", "type": "function", "function": {"name": "bash", "arguments": json.dumps({"command": f"echo {i}"})}}
                 for i in range(3)]
        done = self.run_hxm("go", script=[reply({"role": "assistant", "content": "", "tool_calls": calls}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results(), ["0\n[exit 0]", "1\n[exit 0]", "2\n[exit 0]"])

    def test_a_bad_tool_call_is_reported_to_the_model_not_fatal(self):
        bad_json = {"role": "assistant", "content": None, "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "bash", "arguments": '{"command": "ec'}}]}
        done = self.run_hxm("go", script=[reply(bad_json), call("nope", {}, "c2"), call("read", {}, "c3"), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        results = self.tool_results()
        self.assertIn("not a JSON object", results[0])
        self.assertIn("no tool named nope", results[1])
        self.assertIn('argument "path" must be a string', results[2])

    def test_the_task_can_come_from_stdin(self):
        done = self.run_hxm(stdin="line one\nline two\n", script=[say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.requests[0]["messages"][1]["content"], "line one\nline two")

    def test_no_task_is_an_error(self):
        done = self.run_hxm(stdin="")
        self.assertEqual(done.returncode, 1)
        self.assertIn("no task", done.stderr)

    def test_a_cut_off_reply_is_continued(self):
        done = self.run_hxm("go", script=[say("part one", finish="length"), say("part two")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "part one\npart two\n")
        self.assertIn("cut off", self.model.requests[1]["messages"][-1]["content"])

    def test_a_failed_request_is_retried(self):
        done = self.run_hxm("go", script=[(500, {"error": "busy"}), say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(self.model.requests), 2)
        self.assertEqual(self.model.requests[0], self.model.requests[1])

    def test_a_rejected_request_ends_the_run(self):
        done = self.run_hxm("go", script=[(400, {"error": {"message": "no such model"}})])
        self.assertEqual(done.returncode, 1)
        self.assertIn("no such model", done.stderr)
        self.assertEqual(len(self.model.requests), 1)

    def test_a_long_conversation_is_replaced_by_its_summary(self):
        done = self.run_hxm("the task", script=[call("bash", {"command": "echo big"}, prompt_tokens=200000),
                                                say("SUMMARY TEXT"), say("finished")])
        self.assertEqual(done.returncode, 0, done.stderr)
        _, ask, after = self.model.requests
        self.assertIn("replaced by a summary", ask["messages"][-1]["content"])
        self.assertEqual([m["role"] for m in after["messages"]], ["system", "user"])
        self.assertIn("SUMMARY TEXT", after["messages"][1]["content"])
        self.assertEqual(done.stdout, "finished\n")  # the summary itself is not printed as an answer

    # --- the walls ---

    def test_the_model_cannot_leave_the_project(self):
        with open(os.path.join(self.home, "secret"), "w") as f:
            f.write("TOP-SECRET")
        outside = os.path.join(self.root, "outside.txt")
        done = self.run_hxm("go", script=[
            call("bash", {"command": "cat ~/secret; ls ~"}, "c1"),
            call("read", {"path": os.path.join(self.home, "secret")}, "c2"),
            call("bash", {"command": f"echo x > {outside}; echo x > ~/planted; mkdir /etc/hxm-test"}, "c3"),
            call("write", {"path": outside, "content": "x"}, "c4"),
            call("lua", {"code": f"return host.read('{self.home}/secret')"}, "c5"),
            call("bash", {"command": "echo in > inside.txt && echo t > $TMPDIR/t && cat inside.txt $TMPDIR/t && ls /usr >/dev/null && echo fine"}, "c6"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        results = self.tool_results()
        self.assertEqual(len(results), 6)
        self.assertNotIn("TOP-SECRET", json.dumps(self.model.requests))
        for i in range(5):
            self.assertIn("Permission denied", results[i], i)
        self.assertEqual(results[5], "in\nt\nfine\n[exit 0]")
        self.assertFalse(os.path.exists(outside))
        self.assertFalse(os.path.exists(os.path.join(self.home, "planted")))

    def test_ro_and_rw_open_more(self):
        extra_ro = os.path.join(self.root, "ro")
        extra_rw = os.path.join(self.root, "rw")
        os.makedirs(extra_ro)
        os.makedirs(extra_rw)
        with open(os.path.join(extra_ro, "f"), "w") as f:
            f.write("readable")
        done = self.run_hxm("--ro", extra_ro, "--rw", extra_rw, "go", script=[
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
        done = self.run_hxm("go", script=[call("bash", {"command": "git config user.name"}), say("ok")])
        self.assertEqual(self.tool_results(), ["Test Owner\n[exit 0]"])

    def test_the_home_directory_is_not_a_project(self):
        done = subprocess.run([HXM, "go"], cwd=self.home, env=self.env(), capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 2)
        self.assertIn("refusing to treat", done.stderr)
        self.assertEqual(self.model.requests, [])

    def test_the_api_key_reaches_the_endpoint_and_nothing_else(self):
        done = self.run_hxm("go", OTHER_SECRET_TOKEN="hunter2-token", GIT_CONFIG_KEY_0="not-a-secret", script=[
            call("bash", {"command": "env; tr '\\0' '\\n' < /proc/$PPID/environ; cat /proc/$PPID/cmdline"}, "c1"),
            call("lua", {"code": "local t = {} for k, v in host do t[#t + 1] = k .. '=' .. tostring(v) end "
                                 "return table.concat(t, ' '), host.env('HXM_TEST_KEY'), host.env('OTHER_SECRET_TOKEN')"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.model.headers[0]["Authorization"], "Bearer " + KEY)
        sent = json.dumps(self.model.requests)
        self.assertNotIn(KEY, sent)
        self.assertNotIn("hunter2-token", sent)
        self.assertIn("GIT_CONFIG_KEY_0=not-a-secret", self.tool_results()[0])  # only secrets are removed

    def test_keep_env_keeps_a_variable(self):
        done = self.run_hxm("--keep-env", "KEPT_TOKEN", "go", KEPT_TOKEN="kept-value", script=[
            call("bash", {"command": "echo $KEPT_TOKEN"}), say("ok")])
        self.assertEqual(self.tool_results(), ["kept-value\n[exit 0]"])

    # --- processes ---

    def test_a_command_that_runs_too_long_is_killed_with_its_children(self):
        started = time.monotonic()
        done = self.run_hxm("go", script=[
            call("bash", {"command": "(sleep 300; echo late) & echo $! > child.pid; sleep 300", "timeout": 1}), say("ok")])
        self.assertLess(time.monotonic() - started, 20)
        self.assertIn("[timed out and was killed]", self.tool_results()[0])
        self.assert_gone(int(read(self.project, "child.pid")))

    def test_a_background_process_lives_until_hxm_exits(self):
        done = self.run_hxm("go", script=[
            call("bash", {"command": "sleep 300 > /dev/null 2>&1 & echo $! > bg.pid"}, "c1"),
            call("bash", {"command": "kill -0 $(cat bg.pid) && echo still running"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results()[1], "still running\n[exit 0]")
        self.assert_gone(int(read(self.project, "bg.pid")))

    def test_sigint_stops_the_command_and_the_run(self):
        self.model.script = [call("bash", {"command": "echo $$ > cmd.pid; sleep 300"}), say("never")]
        process = subprocess.Popen([HXM, "go"], cwd=self.project, env=self.env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        pid_file = os.path.join(self.project, "cmd.pid")
        for _ in range(100):
            if os.path.exists(pid_file) and read(pid_file).strip():
                break
            time.sleep(0.05)
        process.send_signal(signal.SIGINT)
        process.communicate(timeout=20)
        self.assertEqual(process.returncode, 130)
        self.assert_gone(int(read(pid_file)))
        self.assertEqual(len(self.model.requests), 1)

    def test_an_interrupted_turn_leaves_a_conversation_that_continues(self):
        import pty
        self.model.script = [say("first answer"), call("bash", {"command": "echo $$ > cmd.pid; sleep 300"}),
                             say("after the interrupt")]
        master, slave = pty.openpty()
        process = subprocess.Popen([HXM], cwd=self.project, env=self.env(), stdin=slave, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, text=True)
        os.close(slave)
        try:
            os.write(master, b"hello\n")
            self.assertEqual(process.stdout.readline(), "first answer\n")
            os.write(master, b"run it\n")
            pid_file = os.path.join(self.project, "cmd.pid")
            for _ in range(100):
                if os.path.exists(pid_file) and read(pid_file).strip():
                    break
                time.sleep(0.05)
            process.send_signal(signal.SIGINT)
            self.assert_gone(int(read(pid_file)))
            os.write(master, b"go on\n")
            self.assertEqual(process.stdout.readline(), "after the interrupt\n")
            os.write(master, b"\x04")  # Ctrl-D
            process.stdout.close()
            self.assertEqual(process.wait(timeout=20), 0)
        finally:
            process.kill()
            os.close(master)
        roles = [m["role"] for m in self.model.requests[2]["messages"]]
        self.assertEqual(roles, ["system", "user", "assistant", "user", "assistant", "tool", "user"])
        self.assertEqual(self.model.requests[2]["messages"][5]["content"], "error: interrupted by the user")

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
        done = self.run_hxm("go", script=[
            call("write", {"path": ".hxm/ext/shout.luau", "content": module}, "c1"),
            call("lua", {"code": "return ext('shout').shout('hey')"}, "c2"),
            say("ok")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.tool_results()[1], "HEY!")
        self.assertNotIn("Extensions in this project", self.model.requests[0]["messages"][0]["content"])
        # A later session is told about it.
        self.run_hxm("again", script=[say("ok")])
        self.assertIn("- shout: shouts a word", self.model.requests[-1]["messages"][0]["content"])
        # With a .hxm directory, the conversation was saved.
        logs = os.listdir(os.path.join(self.project, ".hxm", "log"))
        self.assertTrue(logs)
        saved = json.loads(read(self.project, ".hxm", "log", sorted(logs)[0]))
        self.assertEqual(saved[1]["role"], "user")

    def test_project_notes_reach_the_system_prompt(self):
        os.makedirs(os.path.join(self.project, ".hxm"))
        with open(os.path.join(self.project, ".hxm", "prompt.md"), "w") as f:
            f.write("Run the tests with make check.")
        self.run_hxm("go", script=[say("ok")])
        self.assertIn("Run the tests with make check.", self.model.requests[0]["messages"][0]["content"])

    def test_init_makes_the_project_own_its_loop(self):
        done = self.run_hxm("init")
        self.assertEqual(done.returncode, 0, done.stderr)
        loop = os.path.join(self.project, ".hxm", "loop.luau")
        source = read(loop)
        self.assertIn("function M.main()", source)
        self.assertEqual(self.run_hxm("init").returncode, 2)  # never overwrites
        # The copy is what runs: change it and the behaviour changes.
        with open(loop, "w") as f:
            f.write(source.replace('M.model = host.env("HXM_MODEL") or "deepseek-flash"', 'M.model = "project-model"'))
        self.run_hxm("go", script=[say("ok")])
        self.assertEqual(self.model.requests[0]["model"], "project-model")

    def test_a_broken_project_loop_falls_back_to_the_built_in_one(self):
        os.makedirs(os.path.join(self.project, ".hxm"))
        with open(os.path.join(self.project, ".hxm", "loop.luau"), "w") as f:
            f.write("local M = {\n")
        done = self.run_hxm("go", script=[say("still works")])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, "still works\n")
        self.assertIn("does not compile", done.stderr)
        self.assertIn("using the built-in loop", done.stderr)

    def test_run_executes_a_script(self):
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("print(json.encode(host.args), host.sh('echo hi').out)\n")
        done = self.run_hxm("run", "s.luau", "a", "b")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, '["a","b"]\thi\n\n')

    def test_a_script_error_is_reported(self):
        with open(os.path.join(self.project, "s.luau"), "w") as f:
            f.write("error('it broke')\n")
        done = self.run_hxm("run", "s.luau")
        self.assertEqual(done.returncode, 1)
        self.assertIn("s.luau:1: it broke", done.stderr)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    HXM = os.path.abspath(sys.argv.pop(1))
    unittest.main(verbosity=1)
