This is a small Python log-analysis tool (`logkit/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Add these features:

1. CLI options `--since TS` and `--until TS`, where TS is a UTC timestamp like `2026-01-05T10:00:00Z`. `--since` is inclusive and `--until` is exclusive (use the existing `filters.between`). They combine with `--level`, `--component` and `--summary`. An unparseable TS prints an error message to stderr and makes `main` return 2.
2. CLI option `--json`: instead of the raw lines, print each matching record as one JSON object per line with exactly these keys in this order: `ts` (string formatted like `2026-01-05T10:00:03Z`), `level`, `component`, `fields` (object of strings), `msg`. If `--summary` is also given, the summary wins.
3. A function `render.json_line(record)` that returns that JSON object as a string (no trailing newline); the CLI uses it.

Update or add tests as you see fit.
