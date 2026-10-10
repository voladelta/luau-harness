DOMAIN = "taskq"
CHECK = '''if grep -rnE "[\\"'](queued|running|done|dead)[\\"']" taskq/ --include=*.py | grep -v '^taskq/jobs.py:'; then echo "status string literals outside jobs.py"; fail=1; fi'''

def solve(h):
    h.edit("taskq/jobs.py", "from dataclasses import dataclass\n", "from dataclasses import dataclass\nfrom enum import Enum\n\n\nclass Status(Enum):\n    QUEUED = \"queued\"\n    RUNNING = \"running\"\n    DONE = \"done\"\n    DEAD = \"dead\"\n")
    h.edit("taskq/jobs.py", '    status: str = "queued"  # queued | running | done | dead', "    status: Status = Status.QUEUED")
    p = h.ws / "taskq/queue.py"
    s = p.read_text()
    for a, b in [('"queued"', "Status.QUEUED"), ('"running"', "Status.RUNNING"), ('"done"', "Status.DONE"), ('"dead"', "Status.DEAD")]:
        s = s.replace(a, b)
    s = s.replace("from .jobs import Job\n", "from .jobs import Job, Status\n").replace("out[j.status] = out.get(j.status, 0) + 1", "out[j.status.value] = out.get(j.status.value, 0) + 1")
    p.write_text(s)
    h.edit("taskq/store.py", "from .jobs import Job\n", "from .jobs import Job, Status\n")
    h.edit("taskq/store.py", '{f: getattr(j, f) for f in _FIELDS}', '{f: (j.status.value if f == "status" else getattr(j, f)) for f in _FIELDS}')
    h.edit("taskq/store.py", '        q.jobs[d["id"]] = Job(**{f: d[f] for f in _FIELDS})', '        kw = {f: d[f] for f in _FIELDS}\n        kw["status"] = Status(kw["status"])\n        q.jobs[d["id"]] = Job(**kw)')
