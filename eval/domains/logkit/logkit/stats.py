import math
from collections import Counter


def count_by(records, key):
    return dict(Counter(r[key] for r in records))


def percentile(values, p):
    """Nearest-rank percentile: sorted[ceil(p/100 * n) - 1]."""
    if not values:
        raise ValueError("no values")
    s = sorted(values)
    rank = max(1, math.ceil(p / 100 * len(s)))
    return s[rank - 1]


def latency_summary(records, field="ms"):
    vals = [float(r["fields"][field]) for r in records if field in r["fields"]]
    if not vals:
        return {"count": 0, "p50": None, "p95": None, "max": None}
    return {"count": len(vals), "p50": percentile(vals, 50), "p95": percentile(vals, 95), "max": max(vals)}


def error_rate(records):
    if not records:
        return 0.0
    return sum(1 for r in records if r["level"] == "ERROR") / len(records)
