DOMAIN = "taskq"

def solve(h):
    h.edit("taskq/jobs.py", "from dataclasses import dataclass", "from dataclasses import dataclass, field")
    h.edit("taskq/jobs.py", "    error: str | None = None\n", "    error: str | None = None\n    depends_on: list = field(default_factory=list)\n")
    h.edit("taskq/queue.py", "priority=0, delay=0.0, max_attempts=3):", "priority=0, delay=0.0, max_attempts=3, depends_on=None):\n        for d in depends_on or []:\n            if d not in self.jobs:\n                raise ValueError(f\"unknown dependency: {d}\")")
    h.edit("taskq/queue.py", "self.clock.now() + delay)\n", "self.clock.now() + delay, depends_on=list(depends_on or []))\n")
    h.edit("taskq/queue.py", 'if j.status == "queued" and j.run_at <= now]', 'if j.status == "queued" and j.run_at <= now\n                 and all(self.jobs[d].status == "done" for d in j.depends_on)]')
    h.edit("taskq/queue.py", '''        else:
            job.status = "dead"
''', '''        else:
            job.status = "dead"
            self._kill_dependents(job)

    def _kill_dependents(self, root):
        dead = {root.id}
        changed = True
        while changed:
            changed = False
            for j in self.jobs.values():
                if j.status == "queued" and dead & set(j.depends_on):
                    j.status = "dead"
                    j.error = f"dependency failed: {root.id}"
                    dead.add(j.id)
                    changed = True
''')
    h.edit("taskq/store.py", '"status", "error"]', '"status", "error", "depends_on"]')
    h.edit("taskq/store.py", "Job(**{f: d[f] for f in _FIELDS})", "Job(**{f: d[f] for f in _FIELDS if f in d})")
