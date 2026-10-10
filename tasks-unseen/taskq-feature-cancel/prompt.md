This is a small Python in-memory job queue (`taskq/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Add job cancellation:

1. `JobQueue.cancel(job_id)` marks a queued job as cancelled: its `status` becomes `"cancelled"` and the new field `Job.cancelled_at` (float, default `None`) is set to the queue clock's current time. Cancelled jobs are never returned by `pop_ready` and are never retried. Cancelling a job that is `"running"`, `"done"` or `"dead"` raises `ValueError(f"cannot cancel {status} job: {job_id}")`; cancelling an already cancelled job is a no-op returning the job; an unknown id raises `KeyError`. `cancel` returns the job.
2. `stats()` counts cancelled jobs under `"cancelled"`.
3. `store.dump` / `store.load` round-trip `cancelled_at`; dumps written before this change (no `cancelled_at` key) must still load, with `None`.

Update or add tests as you see fit.
