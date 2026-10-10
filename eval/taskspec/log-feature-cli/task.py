DOMAIN = "logkit"

def solve(h):
    h.edit("logkit/render.py", "from .stats import", "import json\n\nfrom .stats import")
    h.edit("logkit/render.py", "def table(rows, cols):", '''def json_line(record):
    return json.dumps({"ts": fmt_ts(record["ts"]), "level": record["level"], "component": record["component"],
                       "fields": record["fields"], "msg": record["msg"]})


def table(rows, cols):''')
    h.edit("logkit/render.py", "import json\n", "import json\n\nfrom .timeutil import fmt_ts\n")
    h.edit("logkit/cli.py", "from .filters import by_component, by_level", "from .filters import between, by_component, by_level")
    h.edit("logkit/cli.py", "from .render import summary_text", "from .render import json_line, summary_text\nfrom .timeutil import parse_ts")
    h.edit("logkit/cli.py", "    summary = False\n", "    summary = as_json = False\n")
    h.edit("logkit/cli.py", '''        elif opts[i] == "--summary":''', '''        elif opts[i] in ("--since", "--until"):
            try:
                ts = parse_ts(opts[i + 1])
            except (ValueError, IndexError):
                print(f"bad timestamp for {opts[i]}", file=sys.stderr)
                return 2
            records = between(records, start=ts) if opts[i] == "--since" else between(records, end=ts)
            i += 2
        elif opts[i] == "--json":
            as_json = True
            i += 1
        elif opts[i] == "--summary":''')
    h.edit("logkit/cli.py", '''    else:
        for r in records:
            print(r["raw"])''', '''    else:
        for r in records:
            print(json_line(r) if as_json else r["raw"])''')
