#!/usr/bin/env python3
"""Eval gate for a proposed harness-module change (AC5).

Runs the SAME tasks twice: arm A with the module in force when the proposal was made, arm B with the candidate. The
candidate passes only if it does not lose pass rate (> --max-pass-drop) and does not cost more fresh tokens
(> --max-token-increase). The verdict is written to <project>/.harness/proposals/<id>/gate.json, bound to the candidate's
sha256, which `hx upgrade accept` requires. The agent never runs or edits this.

  python3 eval/gate.py --project DIR --proposal ID --tasks tasks --only t1,t2,t3 --runs 2 --model cyberouter/deepseek-v4.1-flash
"""
import argparse, hashlib, json, os, statistics as st, subprocess, sys, tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

def arm(label, override, a, out):
    env = {**os.environ, "EVAL_OVERRIDE": override}
    cmd = [sys.executable, str(ROOT / "eval/run.py"), "--tasks", a.tasks, "--runs", str(a.runs), "--systems", f"hx:{a.model}", "--out", str(out)]
    if a.only: cmd += ["--only", a.only]
    subprocess.run(cmd, env=env, check=True, stdout=subprocess.DEVNULL if a.quiet else None)
    rows = [json.loads(l) for f in Path(out).glob("results.jsonl") for l in open(f)]
    solved = [r for r in rows if r["passed"]]
    per_task = {}
    for r in solved: per_task.setdefault(r["task"], []).append(r["tokens_fresh"])
    return {"label": label, "runs": len(rows), "passed": len(solved), "pass_rate": len(solved) / max(1, len(rows)),
            "fresh_by_task": {t: st.median(v) for t, v in per_task.items()}, "cost": sum(r.get("cost_usd") or 0 for r in rows)}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", default="."); ap.add_argument("--proposal", required=True)
    ap.add_argument("--tasks", default=str(ROOT / "tasks")); ap.add_argument("--only", default="")
    ap.add_argument("--runs", type=int, default=2); ap.add_argument("--model", required=True)
    ap.add_argument("--max-pass-drop", type=float, default=0.02); ap.add_argument("--max-token-increase", type=float, default=0.05)
    ap.add_argument("--quiet", action="store_true"); ap.add_argument("--no-control", action="store_true", help="skip the A/A noise control (faster, noisier)")
    a = ap.parse_args()
    pdir = Path(a.project).resolve() / ".harness/proposals" / a.proposal
    meta = json.loads((pdir / "meta.json").read_text())
    new, base = pdir / "new.luau", pdir / "base.luau"
    sha = hashlib.sha256(new.read_bytes()).hexdigest()
    if sha != meta["new_sha"]: sys.exit("the staged candidate does not match its proposal record")
    work = Path(tempfile.mkdtemp(prefix="gate-"))
    # The arms are independent (separate workspaces and logs), so they run concurrently. The A/A control is the SAME
    # module again: model sampling is noisy, and a change only counts as a regression if it exceeds that noise.
    from concurrent.futures import ThreadPoolExecutor
    with ThreadPoolExecutor(max_workers=3) as pool:
        fa = pool.submit(arm, "current", f"{meta['module']}={base}", a, work / "a")
        fb = pool.submit(arm, "candidate", f"{meta['module']}={new}", a, work / "b")
        fc = None if a.no_control else pool.submit(arm, "current (control)", f"{meta['module']}={base}", a, work / "a2")
        A, B, A2 = fa.result(), fb.result(), (fc.result() if fc else None)
    common = sorted(set(A["fresh_by_task"]) & set(B["fresh_by_task"]))
    ratio = st.median([B["fresh_by_task"][t] / A["fresh_by_task"][t] for t in common]) if common else None
    noise = None
    if A2:
        cc = sorted(set(A["fresh_by_task"]) & set(A2["fresh_by_task"]))
        if cc:
            r = st.median([A2["fresh_by_task"][t] / A["fresh_by_task"][t] for t in cc]); noise = max(r, 1 / r)
    limit = 1 + a.max_token_increase
    reasons, inconclusive = [], False
    if B["pass_rate"] < A["pass_rate"] - a.max_pass_drop: reasons.append(f"pass rate fell {A['pass_rate']:.0%} -> {B['pass_rate']:.0%}")
    if ratio is None: reasons.append("no task solved by both arms, nothing to compare")
    elif ratio > limit:
        if noise is not None and ratio <= noise * limit:
            inconclusive = True
            reasons.append(f"INCONCLUSIVE: candidate used {ratio:.2f}x the fresh tokens, but rerunning the unchanged module differs by {noise:.2f}x; use more tasks or runs")
        else:
            reasons.append(f"fresh tokens rose {ratio:.2f}x (limit {limit:.2f}x" + (f", noise {noise:.2f}x" if noise else "") + ")")
    verdict = {"pass": not reasons, "inconclusive": inconclusive, "noise": noise, "reason": "; ".join(reasons) or "no regression", "candidate_sha": sha, "module": meta["module"],
               "current": {k: A[k] for k in ("runs", "passed", "pass_rate", "cost")}, "candidate": {k: B[k] for k in ("runs", "passed", "pass_rate", "cost")},
               "fresh_token_ratio": ratio, "tasks": common, "model": a.model, "thresholds": {"max_pass_drop": a.max_pass_drop, "max_token_increase": a.max_token_increase}}
    (pdir / "gate.json").write_text(json.dumps(verdict, indent=2))
    print(json.dumps({k: verdict[k] for k in ("pass", "inconclusive", "noise", "reason", "fresh_token_ratio", "current", "candidate")}, indent=2))
    sys.exit(0 if verdict["pass"] else 1)

main()
