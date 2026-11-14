#!/usr/bin/env python3
"""Checks of hx against the real model endpoint. They cost tokens and need the network, so ctest does not run them.

usage: DEEPSEEK_API_KEY=... live_test.py PATH_TO_HX [unittest arguments]
       ANTHROPIC_API_KEY=... HX_MODEL=sonnet live_test.py PATH_TO_HX [unittest arguments]

HX_MODEL, HX_API_URL and HX_KEY_ENV are passed through. Each test gets a fresh project with a .hx directory, so the
conversation is kept in .hx/log/ and the checks can look at the code the model sent, not only at what it said.
The model is not deterministic: a check here asks for what a working loop makes very likely, not for exact words.
"""

import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

HX = None

# A project function that is never ready, and counts its calls in a file: a model may poll it from a loop inside
# one call, which no count of tool calls would see. A call may run 20 seconds and a task 10 replies, so a model
# that will not stop costs minutes, not hours.
STATUS = '''table.insert(M.functions, {
	name = "status",
	usage = "status() -> text: whether the deployment is ready.",
	run = function()
		host.write("status-calls", (host.read("status-calls") or "") .. "x")
		return "not ready yet"
	end,
})
M.max_replies = 10
M.call_seconds = 20'''


class LiveTest(unittest.TestCase):
    def setUp(self):
        model = os.environ.get("HX_MODEL", "")
        claude = model.startswith("claude-") or model in ("opus", "sonnet", "haiku")
        key_env = os.environ.get("HX_KEY_ENV", "ANTHROPIC_API_KEY" if claude else "DEEPSEEK_API_KEY")
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
                              text=True, timeout=900)
        self.assertEqual(done.returncode, status, done.stderr)
        logs = sorted(glob.glob(os.path.join(self.project, ".hx", "log", "*.json")))
        self.log = json.loads(self.read(logs[-1])) if logs else {"messages": []}
        # A working run sends code to the one tool, calls the functions as they are documented, and does not
        # reach for a tool that is not there.
        for result in self.results():
            self.assertFalse(result.startswith(("error: unknown argument", "error: missing argument",
                                                "error: there is no tool")), result)
            self.assertNotRegex(result, r"argument \d+ \(\w+\) must be", "a function was called the wrong way")
        return done.stdout

    def code(self):
        """The code of every call the model made, in order."""
        return [block["input"].get("code", "") for message in self.log["messages"] if message["role"] == "assistant"
                for block in message["content"] if block["type"] == "tool_use"]

    def all_code(self):
        return "\n".join(self.code())

    def results(self):
        return [block["content"] for message in self.log["messages"]
                if message["role"] == "user" and isinstance(message["content"], list)
                for block in message["content"] if block["type"] == "tool_result"]

    def said(self):
        """Every text the loop added to the conversation as the user, after the task."""
        return [block["text"] for message in self.log["messages"][1:] if message["role"] == "user"
                and isinstance(message["content"], list) for block in message["content"] if block["type"] == "text"]

    def test_a_file_is_made(self):
        self.run_hx("create hello.txt containing the single word hi")
        self.assertEqual(self.read("hello.txt").strip(), "hi")

    def test_an_edit_keeps_crlf_line_ends(self):
        # Taking out the empty line needs old text across line ends, which the model writes as \n.
        self.write("app.ini", b"[server]\r\nhost = localhost\r\nport = 8080\r\n\r\n[log]\r\nlevel = info\r\n", "wb")
        self.run_hx("in app.ini, change the server port to 9090 and take out the empty line between the two "
                    "sections. Use the edit function.")
        self.assertEqual(self.read("app.ini", "rb"),
                         b"[server]\r\nhost = localhost\r\nport = 9090\r\n[log]\r\nlevel = info\r\n")
        self.assertIn("edit(", self.all_code())

    def test_a_line_range_is_read_with_read(self):
        self.write("big.txt", "".join(f"row {i} value {i * 7 % 1000}\n" for i in range(1, 5001)))
        answer = self.run_hx("what exactly is written on line 4321 of big.txt? Answer with that line only.")
        self.assertIn(f"row 4321 value {4321 * 7 % 1000}", answer)
        self.assertNotRegex(self.all_code(), r"bash\([^)]*\b(sed|head|tail|awk)\b",
                            "the model read lines with the shell instead of read")

    def test_a_failing_command_is_noticed(self):
        self.write("check.sh", "#!/bin/sh\necho 'all tests passed'\nexit 1\n")
        os.chmod(os.path.join(self.project, "check.sh"), 0o755)
        answer = self.run_hx("run ./check.sh and tell me in one word, PASS or FAIL, whether it succeeded.")
        self.assertIn("FAIL", answer.upper())
        self.assertNotIn("PASS", answer.upper().replace("PASSED", ""))

    def test_low_effort_is_accepted(self):
        answer = self.run_hx("what is 17 * 23? Answer with the number only.", HX_EFFORT="low")
        self.assertIn("391", answer)

    def test_a_long_output_can_be_searched(self):
        self.write("gen.sh", "#!/bin/sh\nfor i in $(seq 1 20000); do echo \"line $i\"; done\necho 'SECRET-IN-THE-MIDDLE' >&2\n"
                   "for i in $(seq 1 20000); do echo \"line $i\"; done\n")
        answer = self.run_hx("run `sh gen.sh 2>&1` exactly once. Its output contains one line that starts with SECRET. "
                             "Tell me that whole line.")
        self.assertIn("SECRET-IN-THE-MIDDLE", answer)

    def test_one_call_does_a_chore_across_many_files(self):
        for i in range(12):
            self.write(f"mod{i}.py", f"import os\n\ndef f{i}():\n    return os.getcwd()\n")
        self.run_hx("in every mod*.py file, replace `os.getcwd()` with `os.path.abspath('.')`. Do it in as few "
                    "tool calls as you can.")
        for i in range(12):
            self.assertIn("os.path.abspath('.')", self.read(f"mod{i}.py"))
        self.assertLessEqual(len(self.code()), 4, self.code())

    def checkpoint(self):
        """The summary the conversation now starts from: a Claude model's compaction block, which the API wrote,
        or the text of the checkpoint another model wrote as a reply."""
        first = self.log["messages"][0]["content"]
        if isinstance(first, list):
            if first[0].get("type") == "compaction":
                return first[0]["content"]
            first = first[0]["text"]
        self.assertIn("written by you:", first, "the conversation was never compacted")
        return first.split("written by you:", 1)[1]

    def test_a_compacted_conversation_goes_on_from_its_checkpoint(self):
        self.overlay("M.compact_at = 1")
        self.write("a.txt", "alpha\nbravo\ncharlie\n")
        self.run_hx("first read a.txt. Then, in a separate call, write b.txt with the lines of a.txt in reverse "
                    "order. Then read b.txt to check it.")
        self.assertEqual(self.read("b.txt").split(), ["charlie", "bravo", "alpha"])
        summary = self.checkpoint()
        for section in ("## Task", "## Files", "## Next step"):
            self.assertIn(section, summary)

    def test_a_fact_found_early_outlives_several_checkpoints_and_they_are_in_english(self):
        # Every call is compacted, so the code has to be carried from checkpoint to checkpoint, and the
        # conversation is in Vietnamese, which the checkpoints are not.
        self.overlay("M.compact_at = 1")
        self.write("code.sh", "#!/bin/sh\necho 'mã truy cập: KX-4471-QZ'\n")
        os.chmod(os.path.join(self.project, "code.sh"), 0o755)
        for name in ("a", "b", "c"):
            self.write(f"{name}.txt", f"tệp {name}\n")
        self.run_hx("Hãy làm từng bước một, mỗi bước một lần gọi công cụ: chạy ./code.sh; rồi đọc a.txt; rồi đọc "
                    "b.txt; rồi đọc c.txt. Cuối cùng ghi mã truy cập mà code.sh in ra vào tệp answer.txt.")
        self.assertEqual(self.read("answer.txt").strip(), "KX-4471-QZ")
        summary = self.checkpoint()
        letters = [ch for ch in summary if ch.isalpha()]
        self.assertGreater(sum(ch.isascii() for ch in letters) / len(letters), 0.9, summary)

    def test_a_status_that_never_changes_is_given_up(self):
        # Told to keep checking, a model polls: across calls, which the reminder stops, or in a loop inside one
        # call, which the time limit of a call ends. Either way it has to stop and say so before its replies run out.
        self.overlay(STATUS)
        answer = self.run_hx("the deployment was just started and takes a while. Keep checking the status function "
                             "until it reports ready, then tell me it is ready.")
        self.assertGreaterEqual(len(self.read("status-calls")), 1)
        self.assertIn("not", answer.lower())
        self.assertFalse(re.search(r"\.hx|debug\.", self.all_code()), "the model tried to change what it checks")

    def skill(self, directory, name, description, body):
        self.write_path(f"{directory}/{name}/SKILL.md", f"---\nname: {name}\ndescription: {description}\n---\n{body}")

    def write_path(self, name, data):
        os.makedirs(os.path.dirname(os.path.join(self.project, name)), exist_ok=True)
        self.write(name, data)

    def test_a_skill_listed_in_the_prompt_is_loaded_and_followed(self):
        self.skill(".claude/skills", "greeting-files", "How greeting files are written in this project. Use whenever "
                   "asked to write a greeting.", "Write the greeting to greetings/NAME.txt, NAME in lower case. The "
                   "greeting is one line in capital letters, and the file's last line is what scripts/footer.sh "
                   "prints.\n")
        # The script is found next to the skill, and the greeting still goes into the project.
        self.write_path(".claude/skills/greeting-files/scripts/footer.sh", "#!/bin/sh\necho '-- hx'\n")
        self.run_hx("write a greeting for Ana")
        self.assertTrue(any("skill(" in code for code in self.code()), "the skill was not loaded")
        text = self.read("greetings/ana.txt")
        self.assertEqual(text.strip().splitlines()[-1], "-- hx")
        self.assertEqual(text.strip().splitlines()[0], text.strip().splitlines()[0].upper())

    def test_a_skill_not_listed_is_found_by_search(self):
        # Past M.skill_index_names the prompt gives only how many skills there are, so the model has to search.
        self.overlay("M.skill_index_full = 0\nM.skill_index_names = 0")
        self.skill(".agents/skills", "ship-it", "Deploy a release of this project to production.",
                   "In this project, deploying means writing the single word DEPLOYED to the file deploy.log. Do "
                   "nothing else.\n")
        for name, about in (("pdf-tools", "Read and merge PDF files."), ("xlsx", "Edit Excel spreadsheets."),
                            ("changelog", "Write the changelog from git history.")):
            self.skill(".agents/skills", name, about, "Not this one.\n")
        self.run_hx("deploy the release")
        code = "\n".join(self.code())
        self.assertIn("skill(", code)
        self.assertNotIn("ship-it", self.code()[0], "the name was not in the prompt, so it cannot be known up front")
        self.assertEqual(self.read("deploy.log").strip(), "DEPLOYED")

    def test_running_out_of_replies_ends_with_a_report(self):
        self.overlay("M.max_replies = 2")
        for name in ("a", "b", "c", "d"):
            self.write(f"{name}.txt", f"{name}\n")
        answer = self.run_hx("read a.txt, b.txt, c.txt and d.txt one at a time, one tool call each, then tell me "
                             "what they hold", status=1)
        self.assertNotEqual(answer.strip(), "")
        final = self.log["messages"][-1]
        self.assertEqual(final["role"], "assistant")
        self.assertEqual([b for b in final["content"] if b["type"] == "tool_use"], [])


def run_all_at_once(hx):
    """Runs every test in its own process at the same time: each waits on the model, not on the machine."""
    import concurrent.futures
    import time
    names = unittest.TestLoader().getTestCaseNames(LiveTest)
    start = time.time()

    def one(name):
        done = subprocess.run([sys.executable, __file__, hx, f"LiveTest.{name}"], capture_output=True, text=True)
        return name, done.returncode, done.stderr, time.time() - start

    failed = 0
    with concurrent.futures.ThreadPoolExecutor(len(names)) as pool:
        for name, code, err, took in pool.map(one, names):
            print(f"{'ok  ' if code == 0 else 'FAIL'} {took:5.0f}s  {name}")
            if code != 0:
                failed += 1
                print("\n".join("      " + line for line in err.strip().splitlines()[-15:]))
    print(f"{len(names) - failed}/{len(names)} passed in {time.time() - start:.0f}s")
    return 1 if failed else 0


if __name__ == "__main__":
    HX = os.path.abspath(sys.argv.pop(1))
    if len(sys.argv) == 1:
        sys.exit(run_all_at_once(HX))
    unittest.main(verbosity=2)
