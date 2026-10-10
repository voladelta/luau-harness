This is a small Python log-analysis tool (`logkit/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Refactor: log records are plain dicts (`r["level"]`, `r["fields"]["ms"]`, ...) passed through every module. Replace them with a dataclass:

- New module `logkit/record.py` with `@dataclass class Record` having the attributes `ts` (datetime), `level`, `component`, `fields` (dict), `msg`, `raw`.
- `parser.parse_line` returns a `Record` (or `None` for malformed lines); `read_records` returns a list of `Record`.
- `filters`, `stats`, `render` and `cli` use attribute access. `stats.count_by(records, key)` keeps taking the attribute name as a string (`"level"`, `"component"`).
- No dict-style access to record keys (`r["level"]`, `r["fields"]`, ...) may remain anywhere in `logkit/`.
- Behaviour, output and the CLI are otherwise unchanged. Update the existing tests.
