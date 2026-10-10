#!/usr/bin/env python3
"""Aggregate results.jsonl (or a directory of shard*/results.jsonl) -> per-task table, medians, P0 gate."""
import json, statistics as st, sys
from collections import defaultdict
from pathlib import Path

arg = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).parent / "results")
files = [arg] if arg.is_file() else sorted(arg.glob("**/results.jsonl"))
rows = [json.loads(l) for f in files for l in f.read_text().splitlines() if l.strip()]
by = defaultdict(list)
for r in rows: by[(r["task"], r["system"])].append(r)
systems = sorted({r["system"] for r in rows}, key=lambda s: (s == "claude", s))
tasks = sorted({r["task"] for r in rows})
med = lambda xs: st.median(xs) if xs else None
f = lambda x, d=0: "-" if x is None else f"{x:.{d}f}"
short = lambda s: s.replace("hx:cyberouter/", "hx:").replace("-flash", "")
cat = lambda t: t.split("-")[1]

def hit(r):  # share of prompt tokens served from the provider's prompt cache
    prompt = r.get("input_tokens", 0) + r.get("cache_creation_tokens", 0) + r.get("cache_read_tokens", 0)
    return r.get("cache_read_tokens", 0) / prompt if prompt else None

def stats(rs):
    ok = [r for r in rs if "harness_error" not in r]
    solved = [r for r in ok if r["passed"]]
    d = {"pass": f"{sum(r['passed'] for r in rs)}/{len(rs)}", "errs": len(rs) - len(ok),
         "tok": med([r["tokens_total"] for r in solved]), "fresh": med([r["tokens_fresh"] for r in solved]),
         "cost": med([r["cost_usd"] for r in solved if r.get("cost_usd") is not None]),
         "turns": med([r["turns"] for r in ok if r.get("turns") is not None]), "wall": med([r["wall_secs"] for r in ok]),
         "hit": med([hit(r) for r in ok if hit(r) is not None])}
    ct = sum(r.get("code_turns", 0) for r in ok if "code_turns" in r)
    if ct:
        d["err"] = sum(r["luau_errors"] for r in ok) / ct
        ops = [o for r in ok for o in r.get("ops_per_turn", [])]
        d["ops"] = sum(ops) / len(ops) if ops else None
    return d

D = {(t, s): stats(by[(t, s)]) for t in tasks for s in systems if by[(t, s)]}
print(f"{'task':<24}{'system':<18}{'pass':<6}{'tok':>8}{'fresh':>8}{'cost$':>8}{'turns':>6}{'luauErr%':>9}{'ops/t':>6}{'wall s':>7}{'cache%':>7}")
for t in tasks:
    for s in systems:
        if (t, s) in D:
            d = D[(t, s)]
            print(f"{t:<24}{short(s):<18}{d['pass']:<6}{f(d['tok']):>8}{f(d['fresh']):>8}{f(d['cost'],4):>8}{f(d['turns'],1):>6}"
                  f"{f(None if d.get('err') is None else d['err']*100):>9}{f(d.get('ops'),1):>6}{f(d['wall']):>7}{f(None if d['hit'] is None else d['hit']*100):>7}")

print("\nPER SYSTEM (medians over tasks with >=1 solve; pass = all runs)")
for s in systems:
    xs = [D[(t, s)] for t in tasks if (t, s) in D]
    sol = [d for d in xs if d["tok"] is not None]
    p = sum(r["passed"] for r in rows if r["system"] == s); n = sum(1 for r in rows if r["system"] == s)
    print(f"  {short(s):<16} pass={p}/{n}  tok={f(med([d['tok'] for d in sol]))} fresh={f(med([d['fresh'] for d in sol]))} "
          f"cost=${f(med([d['cost'] for d in sol if d['cost'] is not None]),4)} turns={f(med([d['turns'] for d in sol]),1)} wall={f(med([d['wall'] for d in sol]))}s "
          f"cache_hit={f(med([d['hit'] for d in sol if d['hit'] is not None]) and 100 * med([d['hit'] for d in sol if d['hit'] is not None]))}%")

def ratios(s, key, only=None):
    out = []
    for t in tasks:
        if only and cat(t) != only: continue
        a, b = D.get((t, s)), D.get((t, "claude"))
        if a and b and a[key] and b[key]: out.append(a[key] / b[key])
    return out

print("\nGATE per hx system vs claude, paired over tasks both solved (ratio of per-task medians; tokens need <= 0.80)")
for s in [x for x in systems if x.startswith("hx:")]:
    rs = [r for r in rows if r["system"] == s and "harness_error" not in r]
    ct = sum(r["code_turns"] for r in rs); er = sum(r["luau_errors"] for r in rs)
    print(f"  {short(s)}")
    for key, lab in (("tok", "total tokens"), ("fresh", "fresh tokens"), ("cost", "cost")):
        parts = "  ".join(f"{c}={f(med(ratios(s, key, c)), 2)}(n={len(ratios(s, key, c))})" for c in ("bug", "feature", "refactor"))
        print(f"    {lab:<13} all={f(med(ratios(s, key)), 2)}(n={len(ratios(s, key))})  {parts}")
    print(f"    luau errors: {er}/{ct} turns = {100 * er / ct if ct else 0:.1f}% (need < 15%)")
