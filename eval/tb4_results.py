#!/usr/bin/env python3
"""Aggregate Terminal-Bench 4.0 experiment runs into one table, over both run-directory layouts.

The TB4 experiments wrote results two ways, and no reader understood both, so results in one layout stayed invisible:

  scheme A (run_task.sh)  runs/<task>-<arm>-<model>-<effort>-<n>/
                          grade/logs/verifier/reward.txt   official reward
                          grade/logs/verifier/ctrf.json    per-test detail -> partial credit
  scheme B (run_mvcc.sh)  runs/<task>-<arm>-<model>-<effort>-<n>/ plus a sibling <same-name>.out
                          .out holds the pytest tail       partial credit only

A run is VOID when hx died instead of finishing: an empty result.json, or a fatal stderr (a plan rate-limit kill, say).
A VOID run still has a graded reward of 0 on disk, because the grader ran against a workspace the agent never wrote
to. Counting those as failures understates every arm they land in, so they are excluded from the cells and reported
separately.

Partial credit (tests passed / total) is kept alongside the binary reward: on an all-or-nothing grader a real effect
can move the test count well before it moves the reward, and that is the only reason the mvcc checkpoint effect was
visible at all.

Usage:
  tb4_results.py --runs DIR [--task T] [--arm A] [--runs-detail] [--json]
"""
import argparse, json, os, re, sys
from collections import defaultdict
from math import comb
from pathlib import Path

MODELS = {"haiku", "sonnet", "opus", "ds41f", "glm53f", "rds41f", "rglm53f", "orhaiku", "eds41f"}
EFFORTS = {"low", "medium", "high", "none"}
FATAL = [
    ("rate-limit", "plan usage limit reached"),
    ("sdk-error", "claude-sdk:"),
    ("hx-error", "runtime error:"),
]
PYTEST_TAIL = re.compile(r"(?:(\d+) failed,\s*)?(\d+) passed")


def parse_name(name):
    """<task>-<arm...>-<model>-<effort>-<n>; arm may contain dashes (hx-base, hx-check2). None if it does not fit."""
    p = name.split("-")
    if len(p) < 4 or not p[-1].isdigit() or p[-3] not in MODELS or p[-2] not in EFFORTS:
        return None
    return {"task": p[0], "arm": "-".join(p[1:-3]) or "base", "model": p[-3], "effort": p[-2], "n": int(p[-1])}


def void_reason(d):
    """Why this run produced no usable result, or None if it completed."""
    rj = d / "result.json"
    if not rj.exists():
        return "no-result.json"
    if rj.stat().st_size == 0:
        err = (d / "stderr.log").read_text(errors="replace") if (d / "stderr.log").exists() else ""
        for label, needle in FATAL:
            if needle in err:
                return label
        return "empty-result"
    try:
        if json.loads(rj.read_text()).get("is_error"):  # arm cc: Claude Code reports its own failures in result.json
            return "cc-error"
    except ValueError:
        pass
    return None


def partial(d):
    """(passed, total) from the richest source available, or (None, None)."""
    ctrf = d / "grade/logs/verifier/ctrf.json"
    if ctrf.exists():
        try:
            s = json.loads(ctrf.read_text())["results"]["summary"]
            return s["passed"], s["tests"]
        except (KeyError, ValueError):
            pass
    for f in (d.with_suffix(".out"), d / "grade.log"):  # scheme B sibling, then scheme A log
        if f.exists():
            m = PYTEST_TAIL.search(f.read_text(errors="replace"))
            if m:
                failed, passed = int(m.group(1) or 0), int(m.group(2))
                return passed, passed + failed
    return None, None


def reward(d):
    f = d / "grade/logs/verifier/reward.txt"
    if f.exists():
        try:
            return float(f.read_text().strip())
        except ValueError:
            return None
    return None


def collect(runs):
    records, unparsed = [], []
    for d in sorted(p for p in runs.iterdir() if p.is_dir()):
        meta = parse_name(d.name)
        if meta is None:
            unparsed.append(d.name)
            continue
        r = dict(meta, dir=d.name, void=void_reason(d))
        r["reward"] = reward(d)
        r["passed"], r["total"] = partial(d)
        # solved: the official reward when the grader wrote one, else a clean sweep of the tests
        if r["void"]:
            r["solved"] = None
        elif r["reward"] is not None:
            r["solved"] = r["reward"] >= 1.0
        elif r["total"]:
            r["solved"] = r["passed"] == r["total"]
        else:
            r["solved"] = None
        records.append(r)
    return records, unparsed


def fisher2(a, b, c, d):
    """Two-tailed Fisher exact p for [[a,b],[c,d]]."""
    n, r1, c1 = a + b + c + d, a + b, a + c
    pr = lambda x: comb(r1, x) * comb(n - r1, c1 - x) / comb(n, c1)
    obs = pr(a)
    return sum(pr(x) for x in range(max(0, c1 - (n - r1)), min(r1, c1) + 1) if pr(x) <= obs + 1e-12)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--runs", default=os.environ.get("TB4_RUNS"), help="run directory (or TB4_RUNS)")
    ap.add_argument("--task", help="only this task prefix")
    ap.add_argument("--arm", help="only this arm")
    ap.add_argument("--baseline", help="arm to test every other arm against (Fisher exact)")
    ap.add_argument("--runs-detail", action="store_true", help="list every run")
    ap.add_argument("--json", action="store_true", help="emit records as JSON")
    a = ap.parse_args()
    if not a.runs:
        sys.exit("--runs or TB4_RUNS required")

    records, unparsed = collect(Path(a.runs))
    sel = [r for r in records if (not a.task or r["task"] == a.task) and (not a.arm or r["arm"] == a.arm)]
    if a.json:
        print(json.dumps({"records": sel, "unparsed": unparsed}, indent=2))
        return

    cells = defaultdict(list)
    for r in sel:
        cells[(r["task"], r["model"], r["effort"], r["arm"])].append(r)

    print(f"{'task':<11}{'model':<8}{'effort':<8}{'arm':<12}{'n':>3}{'solved':>9}{'rate':>7}{'mean tests':>12}  void")
    for k in sorted(cells):
        rs = cells[k]
        live = [r for r in rs if not r["void"]]
        dead = [r for r in rs if r["void"]]
        known = [r for r in live if r["solved"] is not None]
        nsolved = sum(1 for r in known if r["solved"])
        frac = [r["passed"] / r["total"] for r in live if r["total"]]
        rate = f"{nsolved / len(known):.2f}" if known else "-"
        mt = f"{sum(frac) / len(frac):.2f}" if frac else "-"
        vd = ",".join(sorted({r["void"] for r in dead})) if dead else ""
        print(f"{k[0]:<11}{k[1]:<8}{k[2]:<8}{k[3]:<12}{len(known):>3}{f'{nsolved}/{len(known)}':>9}{rate:>7}{mt:>12}  {vd}")

    if a.baseline:
        print()
        for task, model, effort in sorted({(k[0], k[1], k[2]) for k in cells}):
            base = [r for r in cells.get((task, model, effort, a.baseline), []) if r["solved"] is not None]
            if not base:
                continue
            bs = sum(1 for r in base if r["solved"])
            print(f"{task} {model} {effort}: baseline '{a.baseline}' {bs}/{len(base)}")
            for k in sorted(cells):
                if k[:3] != (task, model, effort) or k[3] == a.baseline:
                    continue
                arm = [r for r in cells[k] if r["solved"] is not None]
                if not arm:
                    continue
                s = sum(1 for r in arm if r["solved"])
                p = fisher2(bs, len(base) - bs, s, len(arm) - s)
                print(f"    {k[3]:<12} {s}/{len(arm):<4} p={p:.4f}{'  *' if p < 0.05 else ''}")

    dead = [r for r in sel if r["void"]]
    if dead:
        print(f"\nVOID ({len(dead)} runs excluded above; graded reward on disk is meaningless):")
        for r in sorted(dead, key=lambda r: r["dir"]):
            print(f"  {r['dir']:<42} {r['void']:<14} reward_on_disk={r['reward']}")
    if unparsed and a.runs_detail:
        print(f"\nunparsed names ({len(unparsed)}, not folded into any cell):")
        for n in unparsed:
            print(f"  {n}")
    elif unparsed:
        print(f"\n{len(unparsed)} directories did not match <task>-<arm>-<model>-<effort>-<n> (--runs-detail to list)")

    if a.runs_detail:
        print(f"\n{'dir':<44}{'solved':>7}{'reward':>8}{'tests':>9}")
        for r in sorted(sel, key=lambda r: r["dir"]):
            t = f"{r['passed']}/{r['total']}" if r["total"] else "-"
            print(f"  {r['dir']:<42}{str(r['solved']):>7}{str(r['reward']):>8}{t:>9}")


if __name__ == "__main__":
    main()
