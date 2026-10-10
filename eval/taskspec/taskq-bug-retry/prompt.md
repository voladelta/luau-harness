This is a small Python in-memory job queue (`taskq/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Bug report: a job enqueued with `max_attempts=3` whose handler always raises is executed four times before it is marked dead. It should run exactly three times in total.

Find and fix the bug. Don't change any public function names or signatures.
