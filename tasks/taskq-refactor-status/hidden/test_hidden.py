import json, unittest
from enum import Enum

from taskq.clock import FakeClock
from taskq.jobs import Job, Status
from taskq.queue import JobQueue
from taskq.store import dump, load
from taskq.worker import Worker


class T(unittest.TestCase):
    def test_enum_shape(self):
        self.assertTrue(issubclass(Status, Enum))
        self.assertNotIsInstance(Status.DONE, str)
        self.assertEqual({m.name: m.value for m in Status}, {"QUEUED": "queued", "RUNNING": "running", "DONE": "done", "DEAD": "dead"})
        self.assertIs(Job("j", "n", {}).status, Status.QUEUED)

    def test_lifecycle(self):
        c = FakeClock()
        q = JobQueue(c)
        a = q.enqueue("a", max_attempts=2)
        self.assertIs(a.status, Status.QUEUED)
        self.assertIs(q.pop_ready().status, Status.RUNNING)
        q.fail(a, "x")
        self.assertIs(a.status, Status.QUEUED)
        c.advance(100)
        q.pop_ready()
        q.fail(a, "x")
        self.assertIs(a.status, Status.DEAD)
        b = q.enqueue("b")
        q.complete(q.pop_ready())
        self.assertIs(b.status, Status.DONE)
        self.assertEqual(q.stats(), {"dead": 1, "done": 1})

    def test_worker(self):
        q = JobQueue(FakeClock())
        ok, missing = q.enqueue("ok"), q.enqueue("nope", max_attempts=1)
        Worker(q, {"ok": lambda p: None}).run_until_idle()
        self.assertIs(ok.status, Status.DONE)
        self.assertIs(missing.status, Status.DEAD)
        self.assertEqual(missing.error, "no handler")

    def test_store(self):
        c = FakeClock()
        q = JobQueue(c)
        a = q.enqueue("a")
        q.complete(q.pop_ready())
        text = dump(q)
        self.assertEqual(json.loads(text)["jobs"][0]["status"], "done")
        q2 = load(c, text)
        self.assertIs(q2.jobs[a.id].status, Status.DONE)
        old = '{"n": 1, "jobs": [{"id": "job-1", "name": "a", "payload": {}, "priority": 0, "attempts": 1, "max_attempts": 3, "run_at": 0.0, "status": "dead", "error": "x"}]}'
        self.assertIs(load(c, old).jobs["job-1"].status, Status.DEAD)
