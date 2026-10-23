#!/usr/bin/env python3
"""Run every offline suite within five minutes, including compilation."""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import os
from pathlib import Path
import re
import signal
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]


def stop(process):
    # Shell commands have their own process groups; stop descendants too.
    rows = subprocess.run(["ps", "-axo", "pid=,ppid="], capture_output=True, text=True, timeout=2).stdout
    children = {}
    for row in rows.splitlines():
        pid, parent = map(int, row.split())
        children.setdefault(parent, []).append(pid)
    pending, descendants, groups = [process.pid], [], {process.pid}
    while pending:
        pid = pending.pop()
        descendants.append(pid)
        pending.extend(children.get(pid, []))
        try:
            groups.add(os.getpgid(pid))
        except ProcessLookupError:
            pass
    groups.difference_update({0, 1, os.getpgrp()})
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for group in groups:
            try:
                os.killpg(group, sig)
            except (ProcessLookupError, PermissionError):
                pass
        # macOS may report EPERM for a group whose leader just exited. Individual PIDs cover that race.
        for pid in descendants:
            try:
                os.kill(pid, sig)
            except (ProcessLookupError, PermissionError):
                pass
        if sig == signal.SIGTERM:
            time.sleep(0.1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs", type=int, default=4, choices=range(1, 9))
    args = parser.parse_args()
    started = time.monotonic()
    deadline = started + 290  # Leave cleanup time before five minutes.
    output_lock = threading.Lock()

    def run(name, command, cap=90):
        begin = time.monotonic()
        remaining = min(cap, deadline - begin)
        if remaining <= 0:
            with output_lock:
                print(f"{name:18} FAIL: five-minute budget exhausted", flush=True)
            return False
        process = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   text=True, start_new_session=True)
        try:
            output = process.communicate(timeout=remaining)[0]
            ok = process.returncode == 0
        except subprocess.TimeoutExpired:
            stop(process)
            output = process.communicate(timeout=2)[0]
            output += f"\nFAIL: exceeded {remaining:.0f}s deadline; suite and descendants stopped\n"
            ok = False
        elapsed = time.monotonic() - begin
        summaries = re.findall(r"[^\n]*\d+ passed[^\n]*", output)
        if not summaries and output.rstrip().endswith("OK"):
            match = re.search(r"Ran (\d+) tests", output)
            summaries = [f"{match[1]} passed" if match else "OK"]
        summary = summaries[-1].strip() if summaries else ("passed" if ok else "failed")
        with output_lock:
            print(f"{name:18} {'PASS' if ok else 'FAIL'} {elapsed:6.1f}s  {summary}", flush=True)
            if not ok:
                print(output, flush=True)
        return ok

    if not run("release build", ["cargo", "build", "--release", "--quiet"], cap=150):
        return 1
    results = [run("policy_checks", ["bash", "scripts/policy_checks.sh"], cap=60)]
    # Mock servers bind free ports and every suite uses its own workspaces, so the rest run independently.
    suites = [("Rust tests", ["cargo", "test", "--release", "--quiet"]),
              ("std unit tests", ["target/release/hx", "test", "stdlib/std"])]
    for name in ("extension", "llm", "mcp", "vision", "compact", "harness", "journal", "budget", "caps", "upgrade", "trust"):
        suites.append((name + "_checks", ["bash", f"scripts/{name}_checks.sh"]))
    for name in ("redesign", "custom_tool", "rpi", "history"):
        suites.append((name + "_checks", ["python3", f"scripts/{name}_checks.py"]))

    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for future in as_completed([pool.submit(run, *suite) for suite in suites]):
            results.append(future.result())
    # ar runs four scenarios at a time on its own; beside other suites it starves them past their caps.
    results.append(run("ar", ["python3", "scripts/ar_checks.py"], cap=280))
    elapsed = time.monotonic() - started
    print(f"\n{sum(results)}/{len(results)} suites passed in {elapsed:.1f}s (limit: 5 minutes)", flush=True)
    return 0 if all(results) and elapsed < 300 else 1


if __name__ == "__main__":
    raise SystemExit(main())
