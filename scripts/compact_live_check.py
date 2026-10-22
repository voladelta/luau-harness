#!/usr/bin/env python3
"""Opt-in paid acceptance check for std/compact: does a model still finish the task correctly after its history has been
summarized several times? usage: compact_live_check.py MODEL BASE_URL KEY_ENV [RUNS=3]

Each run reads 12 files one per call under a tight window (36k), so the context is compacted several times. Traps:
exact recall of values seen before a compaction; a rule found mid-run (part06: later values count double) that must
survive the compaction that summarizes it away; a large task message (like an RPI stage packet) that would make a
careless trigger compact every turn. Checks per run, from the report and trace: correct SUM, at least two
compactions, one of them after part06 left the kept tail, no rejected summary, at least two tool calls between
compactions (no thrash). Runs go in parallel; every run must pass."""
import json, os, random, re, subprocess, sys, tempfile, threading
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HX = ROOT / "target/release/hx"


def workspace(seed):
    rng = random.Random(seed)
    words = "alpha beta gamma delta epsilon zeta theta kappa lambda sigma omega".split()
    ws = Path(tempfile.mkdtemp(prefix="hx-compact-live-"))
    (ws / "data").mkdir()
    (ws / ".harness").mkdir()
    (ws / ".harness/init.luau").write_text('require("std/recheck").enabled = false\n')
    total = 0
    for i in range(1, 13):
        n = rng.randint(100, 999)
        total += n * (2 if i >= 7 else 1)
        lines = [" ".join(rng.choice(words) for _ in range(14)) for _ in range(60)]
        lines.insert(rng.randint(5, 55), f"SECRET={n}")
        if i == 6:
            lines.insert(rng.randint(5, 55), "RULE: every SECRET value in part07.txt .. part12.txt counts double in the sum.")
        (ws / f"data/part{i:02d}.txt").write_text("\n".join(lines) + "\n")
    notes = "\n".join(" ".join(rng.choice("module cache index buffer queue lease token shard replica commit".split())
                                for _ in range(12)) for _ in range(300))
    (ws / "task.md").write_text(
        "The directory data/ holds part01.txt .. part12.txt. Each file contains exactly one line of the form SECRET=<number>,"
        " and some files contain RULE lines that change how the sum is computed.\n"
        "Process the files strictly one per tool call, in order: in each call, read ONE file and print its entire contents"
        " (do not search or filter in code, and do not read more than one file per call). After each call, note that"
        " file's SECRET value and any RULE in your reply text.\n"
        "When all twelve are done, answer with the twelve values and the sum, following every RULE, in the form: SUM=<number>.\n\n"
        "## Background notes (reference only; nothing to do here)\n" + notes + "\n")
    return ws, total


def run(model, base_url, key_env, seed, results):
    ws, want = workspace(seed)
    trace = ws / "trace.md"
    p = subprocess.run([str(HX), "run", "--workspace", str(ws), "--prompt-file", str(ws / "task.md"), "--model", model,
                        "--base-url", base_url, "--key-env", key_env, "--context-window", "36000", "--max-output", "6000",
                        "--max-turns", "40", "--max-tokens", "2000000", "--trace", str(trace)],
                       capture_output=True, text=True, env={**os.environ, "HX_TRUST": "1"}, timeout=1800)
    report = {}
    try:
        report = json.loads(p.stdout.strip().splitlines()[-1])
    except Exception:
        pass
    sections = re.findall(r"^### (CODE|COMPACT FAILED|COMPACT)\n(.*)$", trace.read_text() if trace.exists() else "", re.M)
    reads, compacts, gaps, since, after_rule = 0, 0, [], 0, False
    for kind, first in sections:
        if kind == "CODE":
            reads += 1 if "part" in first else 0
            since += 1
        elif kind == "COMPACT":
            compacts += 1
            gaps.append(since)
            since = 0
            after_rule |= reads >= 9  # part06 plus three later reads: it has left a ~2-exchange tail
    m = re.search(r"SUM=(\d+)", report.get("final_answer", ""))
    checks = {
        "correct SUM (exact recall + mid-run rule)": m is not None and int(m.group(1)) == want,
        "at least two compactions": compacts >= 2,
        "a compaction after part06 left the tail": after_rule,
        "no rejected summary": not any(k == "COMPACT FAILED" for k, _ in sections),
        "no thrash (>= 2 tool calls between compactions)": all(g >= 2 for g in gaps[1:]),
    }
    results[seed] = (checks, f"want SUM={want}, got {m.group(0) if m else repr(report.get('final_answer', p.stderr[-300:]))[:120]};"
                     f" turns={report.get('turns')} compactions={compacts} gaps={gaps} rate_limits={report.get('rate_limits')} ws={ws}")


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    model, base_url, key_env = sys.argv[1:4]
    runs = int(sys.argv[4]) if len(sys.argv) > 4 else 3
    results = {}
    threads = [threading.Thread(target=run, args=(model, base_url, key_env, seed, results)) for seed in range(1, runs + 1)]
    for t in threads: t.start()
    for t in threads: t.join()
    ok = True
    for seed in sorted(results):
        checks, info = results[seed]
        print(f"run {seed}: {info}")
        for name, passed in checks.items():
            print(f"  {'ok  ' if passed else 'FAIL'} {name}")
            ok &= passed
    ok &= len(results) == runs
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
