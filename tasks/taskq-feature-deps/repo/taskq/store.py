import json

from .jobs import Job
from .queue import JobQueue

_FIELDS = ["id", "name", "payload", "priority", "attempts", "max_attempts", "run_at", "status", "error"]


def dump(queue):
    return json.dumps({"n": queue._n, "jobs": [{f: getattr(j, f) for f in _FIELDS} for j in queue.jobs.values()]})


def load(clock, text):
    data = json.loads(text)
    q = JobQueue(clock)
    q._n = data["n"]
    for d in data["jobs"]:
        q.jobs[d["id"]] = Job(**{f: d[f] for f in _FIELDS})
    return q
