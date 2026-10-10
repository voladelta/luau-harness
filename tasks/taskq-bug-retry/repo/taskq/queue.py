from .jobs import Job
from .retry import backoff, should_retry


class JobQueue:
    def __init__(self, clock):
        self.clock = clock
        self.jobs = {}
        self._n = 0

    def enqueue(self, name, payload=None, priority=0, delay=0.0, max_attempts=3):
        self._n += 1
        job = Job(f"job-{self._n}", name, payload or {}, priority, 0, max_attempts, self.clock.now() + delay)
        self.jobs[job.id] = job
        return job

    def pop_ready(self):
        """Highest priority first, then earliest run_at, then creation order. Counts an attempt."""
        now = self.clock.now()
        ready = [j for j in self.jobs.values() if j.status == "queued" and j.run_at <= now]
        if not ready:
            return None
        job = min(ready, key=lambda j: (-j.priority, j.run_at, int(j.id.split("-")[1])))
        job.status = "running"
        job.attempts += 1
        return job

    def complete(self, job):
        job.status = "done"
        job.error = None

    def fail(self, job, error):
        job.error = error
        if should_retry(job):
            job.status = "queued"
            job.run_at = self.clock.now() + backoff(job.attempts)
        else:
            job.status = "dead"

    def stats(self):
        out = {}
        for j in self.jobs.values():
            out[j.status] = out.get(j.status, 0) + 1
        return out
