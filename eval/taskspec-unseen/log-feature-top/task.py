DOMAIN = "logkit"

def solve(h):
    h.edit("logkit/stats.py", "def percentile(values, p):", '''def top_n(records, key, n):
    if n <= 0:
        return []
    counts = count_by(records, key)
    return sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:n]


def percentile(values, p):''')
    h.edit("logkit/cli.py", "from .render import summary_text", "from .render import summary_text\nfrom .stats import top_n")
    h.edit("logkit/cli.py", "    summary = False\n", "    summary = False\n    top = None\n")
    h.edit("logkit/cli.py", '''        elif opts[i] == "--summary":''', '''        elif opts[i] == "--top":
            try:
                key, n = opts[i + 1], int(opts[i + 2])
                assert key in ("level", "component")
            except (IndexError, ValueError, AssertionError):
                print("usage: --top level|component N", file=sys.stderr)
                return 2
            top = (key, n)
            i += 3
        elif opts[i] == "--summary":''')
    h.edit("logkit/cli.py", '''    if summary:
        print(summary_text(records))''', '''    if top:
        for value, count in top_n(records, *top):
            print(f"{value} {count}")
    elif summary:
        print(summary_text(records))''')
