This is a small Python log-analysis tool (`logkit/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Add a "top N" report:

1. `stats.top_n(records, key, n)` returns the `n` most common values of a record key (`"level"` or `"component"`, same keys as `count_by`) as a list of `(value, count)` tuples, sorted by count descending and then by value ascending. If there are fewer than `n` distinct values, return them all. `n <= 0` returns `[]`.
2. A CLI option `--top KEY N` (it applies after `--level`/`--component` filtering). Instead of the raw lines it prints one line per result, formatted `f"{value} {count}"`. A KEY other than `level` or `component`, or a non-integer N, prints an error to stderr and makes `main` return 2.

Update or add tests as you see fit.
