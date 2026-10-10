DOMAIN = "logkit"
CHECK = '''if grep -rnE "\\[[\\"'](ts|level|component|fields|msg|raw)[\\"']\\]" logkit/ --include=*.py; then echo "dict-style record access remains"; fail=1; fi'''

def solve(h):
    h.write("logkit/record.py", '''from dataclasses import dataclass, field
from datetime import datetime


@dataclass
class Record:
    ts: datetime
    level: str
    component: str
    fields: dict = field(default_factory=dict)
    msg: str = ""
    raw: str = ""
''')
    h.edit("logkit/parser.py", "from .timeutil import parse_ts", "from .record import Record\nfrom .timeutil import parse_ts")
    h.edit("logkit/parser.py", 'return {"ts": ts, "level": parts[1], "component": parts[2], "fields": fields, "msg": " ".join(msg), "raw": line.rstrip("\\n")}', 'return Record(ts, parts[1], parts[2], fields, " ".join(msg), line.rstrip("\\n"))')
    h.edit("logkit/filters.py", 'LEVELS.index(r["level"])', "LEVELS.index(r.level)")
    h.edit("logkit/filters.py", 'r["component"] == component', "r.component == component")
    h.edit("logkit/filters.py", 'r["ts"] >= start', "r.ts >= start")
    h.edit("logkit/filters.py", 'r["ts"] < end', "r.ts < end")
    h.edit("logkit/filters.py", 'name in r["fields"]', "name in r.fields")
    h.edit("logkit/stats.py", "Counter(r[key] for r in records)", "Counter(getattr(r, key) for r in records)")
    h.edit("logkit/stats.py", 'float(r["fields"][field]) for r in records if field in r["fields"]', "float(r.fields[field]) for r in records if field in r.fields")
    h.edit("logkit/stats.py", 'r["level"] == "ERROR"', 'r.level == "ERROR"')
    h.edit("logkit/cli.py", 'print(r["raw"])', "print(r.raw)")
