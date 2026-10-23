#!/usr/bin/env python3
"""Real terminal and headless checks for per-run permissions and positional prompts."""
import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import tempfile
import time
import unittest

import redesign_checks as harness

HX = str(harness.HX)


def terminal(args, answers=(), env=None, timeout=10):
    """Give hx a real controlling terminal; only the owner side supplies approval answers."""
    child_env = {**harness.ENV, **(env or {})}
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(HX, [HX, *args], child_env)
    output = bytearray()
    answered = 0
    status = None
    deadline = time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            if select.select([fd], [], [], 0.1)[0]:
                try:
                    chunk = os.read(fd, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    chunk = b""
                if not chunk:
                    break
                output.extend(chunk)
                prompts = output.count(b"[y/N]")
                while answered < min(prompts, len(answers)):
                    os.write(fd, (answers[answered] + "\n").encode())
                    answered += 1
            child, status = os.waitpid(pid, os.WNOHANG)
            if child:
                pid = None
                # Drain the terminal's remaining output after exit.
                while select.select([fd], [], [], 0)[0]:
                    try:
                        chunk = os.read(fd, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    output.extend(chunk)
                break
        if pid is not None:
            while time.monotonic() < deadline:
                child, status = os.waitpid(pid, os.WNOHANG)
                if child:
                    pid = None
                    break
                time.sleep(0.01)
            if pid is not None:
                raise AssertionError(f"hx did not exit within {timeout}s: {output.decode(errors='replace')}")
        return os.waitstatus_to_exitcode(status), output.decode(errors="replace")
    finally:
        os.close(fd)
        if pid is not None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)


class Approvals(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="hx-approvals-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.ws = self.root / "workspace"
        (self.ws / ".harness").mkdir(parents=True)
        (self.ws / ".harness/init.luau").write_text('require("std/recheck").enabled = false\n')
        self.outside = self.root / "outside"
        self.outside.mkdir()

    def command(self, code, *flags):
        return ["exec", "--workspace", str(self.ws), *flags, code]

    def headless(self, args):
        return subprocess.run([HX, *args], env=harness.ENV, capture_output=True,
                              text=True, timeout=10, start_new_session=True)

    def grant_records(self):
        log, = (self.ws / ".harness/log").glob("*.jsonl")
        return [r for r in harness.records(log) if r["kind"] == "path_grant"]

    def test_workspace_does_not_ask(self):
        code, out = terminal(self.command('write("a.txt","ok"); print(read("a.txt"))'))
        self.assertEqual(code, 0, out)
        self.assertNotIn("[approval needed]", out)
        self.assertEqual((self.ws / "a.txt").read_text(), "ok")

    def test_outside_directory_is_approved_once(self):
        a, b = self.outside / "a.txt", self.outside / "b.txt"
        code, out = terminal(self.command(f'write("{a}","one"); write("{b}","two"); print(read("{a}"))'), ["y"])
        self.assertEqual(code, 0, out)
        self.assertEqual(out.count("[approval needed]"), 1, out)
        self.assertEqual(b.read_text(), "two")
        self.assertEqual(len(self.grant_records()), 1)
        self.assertEqual(self.grant_records()[0]["source"], "prompt")

    def test_denial_does_not_write_or_grant(self):
        target = self.outside / "no.txt"
        code, out = terminal(self.command(f'print(pcall(write,"{target}","no"))'), ["n"])
        self.assertEqual(code, 0, out)
        self.assertIn("user declined", out)
        self.assertFalse(target.exists())
        self.assertEqual(self.grant_records(), [])

    def test_headless_refuses_without_yes(self):
        target = self.outside / "no.txt"
        result = self.headless(self.command(f'print(pcall(write,"{target}","no"))'))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("no controlling terminal", result.stdout)
        self.assertFalse(target.exists())

    def test_yes_grants_without_asking_and_preserves_configuration(self):
        policy = self.ws / ".harness/policy.toml"
        policy.write_text('[approval]\nmode = "deny"\n')
        target = self.outside / "auto.txt"
        code, out = terminal(self.command(f'write("{target}","yes"); print(read("{target}"))', "--yes"))
        self.assertEqual(code, 0, out)
        self.assertNotIn("[approval needed]", out)
        self.assertEqual(target.read_text(), "yes")
        self.assertEqual(policy.read_text(), '[approval]\nmode = "deny"\n')
        self.assertEqual(self.grant_records()[0]["source"], "auto")

    def test_grants_do_not_leak_to_siblings_or_new_runs(self):
        allowed = self.outside / "allowed"
        sibling = self.outside / "sibling"
        allowed.mkdir()
        sibling.mkdir()
        a, b = allowed / "a", sibling / "b"
        code, out = terminal(self.command(f'write("{a}","yes"); print(pcall(write,"{b}","no"))'), ["y", "n"])
        self.assertEqual(code, 0, out)
        self.assertEqual(out.count("[approval needed]"), 2, out)
        self.assertFalse(b.exists())
        result = self.headless(self.command(f'print(pcall(write,"{allowed / "new"}","no"))'))
        self.assertIn("no controlling terminal", result.stdout)
        self.assertFalse((allowed / "new").exists())

    def test_read_approval_does_not_grant_writes(self):
        target = self.outside / "read.txt"
        target.write_text("original")
        code, out = terminal(self.command(f'print(read("{target}")); print(pcall(write,"{target}","changed"))'), ["y", "n"])
        self.assertEqual(code, 0, out)
        self.assertEqual(out.count("[approval needed]"), 2, out)
        self.assertEqual(target.read_text(), "original")

    def test_file_read_grant_does_not_expand_when_replaced_by_a_directory(self):
        target = self.outside / "read-target"
        target.write_text("first")
        code = (f'print(read("{target}")); '
                f'sh("rm {target}; mkdir {target}; echo hidden > {target}/child.txt"); '
                f'print(pcall(search,"hidden",{{path="{target}"}}))')
        status, out = terminal(self.command(code), ["y", "n"])
        self.assertEqual(status, 0, out)
        self.assertEqual(out.count("[approval needed]"), 2, out)
        self.assertIn("user declined", out)

    def test_standing_write_roots_do_not_ask(self):
        (self.ws / ".harness/policy.toml").write_text(f'[fs]\nwrite = ["{self.outside}"]\n')
        target = self.outside / "configured.txt"
        code, out = terminal(self.command(f'write("{target}","ok"); print(read("{target}"))'))
        self.assertEqual(code, 0, out)
        self.assertNotIn("[approval needed]", out)
        self.assertEqual(self.grant_records(), [])

    def test_yes_preserves_protected_paths_secrets_and_symlinks(self):
        (self.ws / "secrets.toml").write_text("do-not-read")
        (self.ws / "linked").symlink_to(self.ws / "secrets.toml")
        code = ('print(pcall(write,".harness/new.toml","no")); print(pcall(write,"hx.lock","no")); '
                'print(pcall(read,"secrets.toml")); print(pcall(edit,"secrets.toml","do","no")); '
                'print(pcall(write,"linked","no")); print(#search("do-not-read"))')
        status, out = terminal(self.command(code, "--yes"))
        self.assertEqual(status, 0, out)
        self.assertEqual(sum(line.startswith("false\t") for line in out.splitlines()), 5, out)
        self.assertNotIn("[approval needed]", out)
        self.assertNotIn("do-not-read", out)
        self.assertFalse((self.ws / "hx.lock").exists())
        self.assertEqual(self.grant_records(), [])

    def test_search_through_an_alias_cannot_expose_sealed_state(self):
        sealed = self.ws / ".harness/ar/run"
        sealed.mkdir(parents=True)
        (sealed / "ledger.txt").write_text("sealed-result-marker")
        (self.ws / "harness-alias").symlink_to(self.ws / ".harness", target_is_directory=True)
        code = 'print(#search("sealed-result-marker",{path="harness-alias",literal=true}))'
        result = self.headless(self.command(code, "--yes"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], "0", result.stdout)

    def test_external_command_approval_and_yes(self):
        code = 'print(sh("echo approved # git push").stdout)'
        status, out = terminal(self.command(code), ["y"])
        self.assertEqual(status, 0, out)
        self.assertEqual(out.count("[approval needed]"), 1, out)
        status, out = terminal(self.command(code, "--yes"))
        self.assertEqual(status, 0, out)
        self.assertNotIn("[approval needed]", out)
        self.assertIn("approved", out)

    def test_yes_keeps_the_scrubbed_environment(self):
        for flags in ((), ("--yes",)):
            result = self.headless(self.command('print(sh("env").stdout)', *flags))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("HX_TEST_KEY", result.stdout)
            self.assertNotIn("local-mock-only", result.stdout)

    def test_yes_cannot_enable_mutations_in_a_read_batch(self):
        target = self.outside / "batch.txt"
        code = f'print(pcall(batch, {{{{op="write", args={{"{target}","no"}}}}}}))'
        result = self.headless(self.command(code, "--yes"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("false", result.stdout)
        self.assertFalse(target.exists())
        self.assertEqual(self.grant_records(), [])

    def run_args(self, url, prompt="Do the scripted work", *flags):
        return ["run", prompt, "--workspace", str(self.ws), "--model", "mock",
                "--base-url", url, "--key-env", "HX_TEST_KEY", *flags]

    def test_positional_prompt_and_autopilot(self):
        target = self.outside / "from-run.txt"
        with harness.provider(lambda body, i: harness.tool(f'write("{target}","done")') if i == 0 else "finished") as (url, calls):
            result = self.headless(self.run_args(url, "Fix something --yes is part of this text", "--yes"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)["finished"], result.stdout)
        self.assertEqual(target.read_text(), "done")
        self.assertIn("Fix something --yes is part of this text", json.dumps(calls[0]))

    def test_resume_restores_an_interactive_grant(self):
        a, b = self.outside / "first.txt", self.outside / "second.txt"
        first = harness.tool(f'write("{a}","first")')
        # Find the first committed controller checkpoint after the file operation.
        control = self.root / "control"
        control.mkdir()
        with harness.provider(lambda body, i: first if i == 0 else "done") as (url, _):
            args = self.run_args(url, "Record a checkpoint", "--yes")
            args[args.index(str(self.ws))] = str(control)
            result = self.headless(args)
            self.assertEqual(result.returncode, 0, result.stderr)
        log, = (control / ".harness/log").glob("*.jsonl")
        records = harness.records(log)
        outcome = next(r["seq"] for r in records if r["kind"] == "outcome" and r["tool"] == "write")
        boundary = next(r["seq"] for r in records if r["kind"] == "checkpoint" and r["lane"] == "root" and r["seq"] > outcome)
        with harness.provider(lambda body, i: first if i == 0 else harness.tool(f'write("{b}","second")') if i == 1 else "done") as (url, _):
            status, out = terminal(self.run_args(url, "Record a checkpoint"), ["y"], env={"HX_TEST_CRASH_AFTER_RECORD": str(boundary)})
            self.assertNotEqual(status, 0, out)
            self.assertEqual(out.count("[approval needed]"), 1, out)
            log, = (self.ws / ".harness/log").glob("*.jsonl")
            status, out = terminal(["run", "--workspace", str(self.ws), "--resume", str(log)])
        self.assertEqual(status, 0, out)
        self.assertNotIn("[approval needed]", out)
        self.assertEqual(b.read_text(), "second")
        self.assertEqual(len(self.grant_records()), 1)

    def test_deadline_cancels_an_unanswered_permission_request(self):
        target = self.outside / "late.txt"
        with harness.provider(lambda body, i: harness.tool(f'write("{target}","late")')) as (url, _):
            started = time.monotonic()
            status, out = terminal(self.run_args(url, "Wait for approval", "--max-wall", "0.5"))
        self.assertEqual(status, 0, out)
        self.assertLess(time.monotonic() - started, 3)
        self.assertIn("[approval needed]", out)
        self.assertFalse(target.exists())

    def test_invalid_invocations_fail_before_opening_a_journal(self):
        for flags in (["one", "two"], ["one", "--prompt-file", "missing"], [""],
                      ["one", "--yolo"], ["one", "--yes", "--approval", "deny"]):
            result = self.headless(["run", "--workspace", str(self.ws), *flags])
            self.assertEqual(result.returncode, 2, (flags, result.stderr))
            self.assertFalse((self.ws / ".harness/log").exists(), flags)


if __name__ == "__main__":
    unittest.main(verbosity=2)
