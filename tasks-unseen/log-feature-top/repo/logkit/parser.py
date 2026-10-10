import shlex

from .timeutil import parse_ts

LEVELS = ["DEBUG", "INFO", "WARN", "ERROR"]


def parse_line(line):
    """`<ts> <LEVEL> <component> key=value ... ["message"]` -> record dict, or None if malformed."""
    parts = line.strip().split(None, 3)
    if len(parts) < 3 or parts[1] not in LEVELS:
        return None
    try:
        ts = parse_ts(parts[0])
    except ValueError:
        return None
    fields, msg = {}, []
    for tok in shlex.split(parts[3]) if len(parts) == 4 else []:
        if "=" in tok:
            k, v = tok.split("=", 1)
            fields[k] = v
        else:
            msg.append(tok)
    return {"ts": ts, "level": parts[1], "component": parts[2], "fields": fields, "msg": " ".join(msg), "raw": line.rstrip("\n")}


def read_records(path):
    with open(path) as f:
        return [r for r in map(parse_line, f) if r]
