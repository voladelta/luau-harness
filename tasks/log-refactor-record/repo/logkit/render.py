from .stats import count_by, error_rate, latency_summary


def table(rows, cols):
    widths = [max(len(str(c)), *(len(str(r[c])) for r in rows)) if rows else len(str(c)) for c in cols]
    lines = ["  ".join(str(c).ljust(w) for c, w in zip(cols, widths))]
    lines += ["  ".join(str(r[c]).ljust(w) for c, w in zip(cols, widths)) for r in rows]
    return "\n".join(lines)


def summary_text(records):
    lat = latency_summary(records)
    out = [f"records: {len(records)}", f"error rate: {error_rate(records):.1%}"]
    out.append(table([{"level": k, "count": v} for k, v in sorted(count_by(records, "level").items())], ["level", "count"]))
    if lat["count"]:
        out.append(f"latency ms: p50={lat['p50']:g} p95={lat['p95']:g} max={lat['max']:g}")
    return "\n".join(out)
