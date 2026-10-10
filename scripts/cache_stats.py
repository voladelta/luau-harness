#!/usr/bin/env python3
"""Per-turn prompt-cache hit ratio from an hx event log: cache_stats.py .harness/log/<session>.jsonl
Turn 1 should be ~0%; later turns should be high. A flat 0% means the prefix is not cacheable (too short, or it changes)."""
import json, sys
n = 0
for line in open(sys.argv[1]):
    r = json.loads(line)
    if r.get("kind") != "llm" or r.get("depth", 0) != 0: continue
    n += 1
    prompt = r["input"] + r["cache_write"] + r["cache_read"]
    print(f"turn {n:>3}  prompt={prompt:>7}  read={r['cache_read']:>7}  write={r['cache_write']:>7}  hit={100 * r['cache_read'] / prompt if prompt else 0:5.1f}%")
