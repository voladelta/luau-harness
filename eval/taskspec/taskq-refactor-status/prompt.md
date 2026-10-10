This is a small Python in-memory job queue (`taskq/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Refactor: job statuses are bare strings (`"queued"`, `"running"`, `"done"`, `"dead"`) scattered across modules. Replace them with an enum:

- In `taskq/jobs.py` add `class Status(Enum)` (a plain `enum.Enum`, not a `str` subclass) with members `QUEUED`, `RUNNING`, `DONE`, `DEAD` whose values are the old strings. `Job.status` holds a `Status` and defaults to `Status.QUEUED`.
- `queue.py`, `worker.py` and `store.py` use the enum members. The string literals `"queued"`, `"running"`, `"done"` and `"dead"` may appear only in `jobs.py`.
- `JobQueue.stats()` still returns a dict keyed by the plain strings (`{"done": 1}`).
- `store.dump` still writes the plain string (`"status": "done"`) and `store.load` converts it back to `Status`, so existing dump files still load.
- Behaviour is otherwise unchanged. Update the existing tests.
