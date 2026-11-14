#!/usr/bin/env python3
"""Checks of hx against the real model endpoint. They cost tokens and need the network, so ctest does not run them.

usage: DEEPSEEK_API_KEY=... live_test.py PATH_TO_HX [unittest arguments]

HX_MODEL, HX_API_URL and HX_KEY_ENV are passed through. Each test gets a fresh project with a .hx directory, so the
conversation is kept in .hx/log/ and the checks can look at the calls the model made, not only at what it said.
The model is not deterministic: a check here asks for what a working loop makes very likely, not for exact words.
"""

import glob
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HX = None


class LiveTest(unittest.TestCase):
    def setUp(self):
        key_env = os.environ.get("HX_KEY_ENV", "DEEPSEEK_API_KEY")
        if not os.environ.get(key_env):
            self.skipTest(f"no {key_env}")
        self.project = tempfile.mkdtemp(prefix="hx-live-")
        os.makedirs(os.path.join(self.project, ".hx"))

    def tearDown(self):
        shutil.rmtree(self.project, ignore_errors=True)

    def write(self, name, data, mode="w"):
        with open(os.path.join(self.project, name), mode) as f:
            f.write(data)

    def read(self, name, mode="r"):
        with open(os.path.join(self.project, name), mode) as f:
            return f.read()

    def overlay(self, code):
        """Runs the built-in loop with `code` applied to it, as a project's .hx/loop.luau would."""
        self.write(".hx/loop.luau", f"local M = assert(host.load(host.default_loop, '=loop'))()\n{code}\nreturn M\n")

    def run_hx(self, task, status=0, **env):
        done = subprocess.run([HX, "-q", task], cwd=self.project, env={**os.environ, **env}, capture_output=True,
                              text=True, timeout=600)
        self.assertEqual(done.returncode, status, done.stderr)
        logs = sorted(glob.glob(os.path.join(self.project, ".hx", "log", "*.json")))
        self.log = json.loads(self.read(logs[-1])) if logs else {"messages": []}
        # No call of a working run should be refused for its argument names.
        for result in self.results():
            self.assertFalse(result.startswith(('error: unknown argument "', 'error: missing argument "')), result)
        return done.stdout

    def said(self):
        """Every text the loop added to the conversation as the user, after the task."""
        return [block["text"] for message in self.log["messages"][1:] if message["role"] == "user"
                and isinstance(message["content"], list) for block in message["content"] if block["type"] == "text"]

    def calls(self, name=None):
        return [block for message in self.log["messages"] if message["role"] == "assistant"
                for block in message["content"] if block["type"] == "tool_use" and name in (None, block["name"])]

    def results(self):
        return [block["content"] for message in self.log["messages"]
                if message["role"] == "user" and isinstance(message["content"], list)
                for block in message["content"] if block["type"] == "tool_result"]

    def test_a_file_is_made(self):
        self.run_hx("create hello.txt containing the single word hi")
        self.assertEqual(self.read("hello.txt").strip(), "hi")

    def test_an_edit_keeps_crlf_line_ends(self):
        # Taking out the empty line needs an old_string across line ends, which the model writes as \n.
        self.write("app.ini", b"[server]\r\nhost = localhost\r\nport = 8080\r\n\r\n[log]\r\nlevel = info\r\n", "wb")
        self.run_hx("in app.ini, change the server port to 9090 and take out the empty line between the two "
                    "sections. Use the edit tool.")
        self.assertEqual(self.read("app.ini", "rb"),
                         b"[server]\r\nhost = localhost\r\nport = 9090\r\n[log]\r\nlevel = info\r\n")
        self.assertTrue(any("\n" in c["input"].get("old_string", "") for c in self.calls("edit")), self.calls())
        self.assertNotIn("not found", "".join(self.results()))

    def test_a_line_range_is_read_with_the_read_tool(self):
        self.write("big.txt", "".join(f"row {i} value {i * 7 % 1000}\n" for i in range(1, 5001)))
        answer = self.run_hx("what exactly is written on line 4321 of big.txt? Answer with that line only.")
        self.assertIn(f"row 4321 value {4321 * 7 % 1000}", answer)
        shell_reads = [c["input"].get("command", "") for c in self.calls("bash")
                       if any(word in c["input"].get("command", "") for word in ("sed ", "head ", "tail ", "awk "))]
        self.assertEqual(shell_reads, [], "the model read lines with the shell instead of the read tool")
        self.assertTrue(any((c["input"].get("offset") or 0) > 1 for c in self.calls("read")), self.calls())

    def test_a_failing_command_is_noticed(self):
        self.write("check.sh", "#!/bin/sh\necho 'all tests passed'\nexit 1\n")
        os.chmod(os.path.join(self.project, "check.sh"), 0o755)
        answer = self.run_hx("run ./check.sh and tell me in one word, PASS or FAIL, whether it succeeded.")
        self.assertIn("FAIL", answer.upper())
        self.assertNotIn("PASS", answer.upper().replace("PASSED", ""))

    def test_low_effort_is_accepted(self):
        answer = self.run_hx("what is 17 * 23? Answer with the number only.", HX_EFFORT="low")
        self.assertIn("391", answer)

    def test_long_output_can_be_followed_to_its_file(self):
        self.write("gen.sh", "#!/bin/sh\nfor i in $(seq 1 20000); do echo \"line $i\"; done\necho 'SECRET-IN-THE-MIDDLE' >&2\n"
                   "for i in $(seq 1 20000); do echo \"line $i\"; done\n")
        answer = self.run_hx("run `sh gen.sh 2>&1` exactly once. Its output contains one line that starts with SECRET. "
                             "Tell me that whole line.")
        self.assertIn("SECRET-IN-THE-MIDDLE", answer)
        self.assertEqual(len([c for c in self.calls("bash") if "gen.sh" in c["input"].get("command", "")]), 1)

    def test_a_compacted_conversation_goes_on_from_its_checkpoint(self):
        self.overlay("M.compact_at = 1")
        self.write("a.txt", "alpha\nbravo\ncharlie\n")
        self.run_hx("first read a.txt. Then, in a separate step, write b.txt with the lines of a.txt in reverse "
                    "order. Then read b.txt to check it.")
        self.assertEqual(self.read("b.txt").split(), ["charlie", "bravo", "alpha"])
        first = self.log["messages"][0]["content"]
        first = first if isinstance(first, str) else first[0]["text"]
        self.assertIn("checkpoint", first)
        for section in ("## Task", "## Files", "## Next step"):
            self.assertIn(section, first)


    def test_a_fact_found_early_outlives_several_checkpoints_and_they_are_in_english(self):
        # Every tool round is compacted, so the code has to be carried from checkpoint to checkpoint, and the
        # conversation is in Vietnamese, which the checkpoints are not.
        self.overlay("M.compact_at = 1")
        self.write("code.sh", "#!/bin/sh\necho 'mã truy cập: KX-4471-QZ'\n")
        os.chmod(os.path.join(self.project, "code.sh"), 0o755)
        for name in ("a", "b", "c"):
            self.write(f"{name}.txt", f"tệp {name}\n")
        self.run_hx("Hãy làm từng bước một, mỗi bước một lệnh: chạy ./code.sh; rồi đọc a.txt; rồi đọc b.txt; rồi "
                    "đọc c.txt. Cuối cùng ghi mã truy cập mà code.sh in ra vào tệp answer.txt.")
        self.assertEqual(self.read("answer.txt").strip(), "KX-4471-QZ")
        checkpoints = [t for t in self.said()] + [self.log["messages"][0]["content"]]
        checkpoints = [c if isinstance(c, str) else c[0]["text"] for c in checkpoints]
        last = [c for c in checkpoints if "checkpoint" in c][-1]
        summary = last.split("written by you:", 1)[1]
        ascii_letters = sum(ch.isascii() for ch in summary if ch.isalpha())
        letters = sum(ch.isalpha() for ch in summary)
        self.assertGreater(ascii_letters / letters, 0.9, summary)

    def test_a_status_polled_between_sleeps_is_given_up_after_a_reminder(self):
        # Told to keep checking, the model polls and sleeps between polls; without the reminder it goes on until
        # it runs out of replies. Sleep returns at once here, and the model sees the result it would have seen.
        self.overlay("""table.insert(M.tools, {
	name = "status",
	description = "Reports whether the deployment is ready.",
	parameters = { type = "object", properties = {} },
	run = function() return "not ready yet" end,
})
M.max_replies = 25
local bash = M.tools[1].run
M.tools[1].run = function(args)
	return bash({ command = "sleep() { :; }; " .. args.command, timeout = args.timeout })
end""")
        answer = self.run_hx("the deployment was just started and takes a while. Keep checking the status tool until "
                             "it reports ready, then tell me it is ready.")
        polls = len(self.calls("status"))
        self.assertGreaterEqual(polls, 3, "the model did not poll, so this run says nothing about the reminder")
        self.assertTrue([t for t in self.said() if "exact same tool call" in t])
        self.assertLessEqual(polls, 5)
        self.assertIn("not", answer.lower())

    def test_running_out_of_replies_ends_with_a_report(self):
        self.overlay("M.max_replies = 2")
        for name in ("a", "b", "c", "d"):
            self.write(f"{name}.txt", f"{name}\n")
        answer = self.run_hx("read a.txt, b.txt, c.txt and d.txt one at a time, one call per reply, then tell me "
                             "what they hold", status=1)
        self.assertNotEqual(answer.strip(), "")
        final = self.log["messages"][-1]
        self.assertEqual(final["role"], "assistant")
        self.assertEqual([b for b in final["content"] if b["type"] == "tool_use"], [])


if __name__ == "__main__":
    HX = os.path.abspath(sys.argv.pop(1))
    unittest.main(verbosity=2)
