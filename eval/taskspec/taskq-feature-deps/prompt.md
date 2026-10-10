This is a small Python in-memory job queue (`taskq/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Add job dependencies:

1. `JobQueue.enqueue(..., depends_on=None)` takes a list of job ids; the new `Job.depends_on` field holds them (default empty list). An id that is not in the queue raises `ValueError("unknown dependency: <id>")`.
2. `pop_ready` must not return a queued job until every job it depends on has status `"done"`.
3. When a job becomes `"dead"` through `fail`, every queued job that depends on it, directly or through other queued jobs, also becomes `"dead"` immediately, with `error` set to `"dependency failed: <id of the job that died>"` (the same id for all transitive dependents). Their attempts stay at 0. A job that is merely retrying (queued again) does not affect its dependents.
4. `store.dump` / `store.load` must round-trip `depends_on`. Dumps written before this change (no `depends_on` key) must still load, with an empty list.

Update or add tests as you see fit.
