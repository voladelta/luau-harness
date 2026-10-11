"""Render a Result as an aligned text table."""


def cell(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, float):
        return f"{v:.2f}"
    return str(v)


def render(result):
    rows = [[cell(v) for v in r] for r in result.rows]
    widths = [max([len(c)] + [len(r[i]) for r in rows]) for i, c in enumerate(result.columns)]
    line = lambda vals: " | ".join(v.ljust(w) for v, w in zip(vals, widths)).rstrip()
    out = [line(result.columns), "-+-".join("-" * w for w in widths)]
    out += [line(r) for r in rows]
    out.append(f"({len(rows)} row{'s' if len(rows) != 1 else ''})")
    return "\n".join(out)
