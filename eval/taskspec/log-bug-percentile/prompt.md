This is a small Python log-analysis tool (`logkit/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Bug report: the latency percentiles printed by `--summary` are too high on some inputs. For example, with exactly 20 requests whose latencies are 1..20 ms, p95 is reported as 20 and not 19, and the p50 of four values 1,2,3,4 is reported as 3. The percentile function is supposed to use the nearest-rank method.

Find and fix the bug. Don't change any public function names or signatures.
