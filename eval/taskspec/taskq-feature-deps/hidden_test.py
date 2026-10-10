import json, unittest

from taskq.clock import FakeClock
from taskq.queue import JobQueue
from taskq.store import dump, load
from taskq.worker import Worker


class T(unittest.TestCase):
    def test_blocked_until_done(self):
        q = JobQueue(FakeClock())
        a = q.enqueue("a")
        b = q.enqueue("b", depends_on=[a.id], priority=10)
        self.assertEqual(b.depends_on, [a.id])
        self.assertEqual(q.pop_ready().id, a.id)
        self.assertIsNone(q.pop_ready())  # a is running, b still blocked
        q.complete(a)
        self.assertEqual(q.pop_ready().id, b.id)

    def test_multiple_deps(self):
        q = JobQueue(FakeClock())
        a, b = q.enqueue("a"), q.enqueue("b")
        c = q.enqueue("c", depends_on=[a.id, b.id], priority=9)
        q.complete(q.pop_ready())
        self.assertNotEqual(q.pop_ready().id, c.id)
        q.complete(b)
        self.assertEqual(q.pop_ready().id, c.id)

    def test_default_is_independent(self):
        q = JobQueue(FakeClock())
        self.assertEqual(q.enqueue("a").depends_on, [])
        self.assertIsNot(q.enqueue("a").depends_on, q.enqueue("b").depends_on)

    def test_death_cascades(self):
        c = FakeClock()
        q = JobQueue(c)
        a = q.enqueue("a", max_attempts=1)
        b = q.enqueue("b", depends_on=[a.id])
        d = q.enqueue("d", depends_on=[b.id])
        free = q.enqueue("free", priority=-5)
        q.fail(q.pop_ready(), "boom")
        for j in (b, d):
            self.assertEqual((j.status, j.error, j.attempts), ("dead", f"dependency failed: {a.id}", 0))
        self.assertEqual(free.status, "queued")
        self.assertEqual(q.stats(), {"dead": 3, "queued": 1})

    def test_retry_does_not_cascade(self):
        q = JobQueue(FakeClock())
        a = q.enqueue("a", max_attempts=2)
        b = q.enqueue("b", depends_on=[a.id])
        q.fail(q.pop_ready(), "boom")
        self.assertEqual((a.status, b.status), ("queued", "queued"))

    def test_unknown_dependency(self):
        with self.assertRaisesRegex(ValueError, "unknown dependency: job-99"):
            JobQueue(FakeClock()).enqueue("a", depends_on=["job-99"])

    def test_store(self):
        c = FakeClock()
        q = JobQueue(c)
        a = q.enqueue("a")
        b = q.enqueue("b", depends_on=[a.id])
        q2 = load(c, dump(q))
        self.assertEqual(q2.jobs[b.id].depends_on, [a.id])
        old = json.loads(dump(q))
        for j in old["jobs"]:
            del j["depends_on"]
        q3 = load(c, json.dumps(old))
        self.assertEqual(q3.jobs[b.id].depends_on, [])

    def test_worker_order(self):
        c = FakeClock()
        q = JobQueue(c)
        order = []
        a = q.enqueue("t", {"n": "a"}, priority=0)
        q.enqueue("t", {"n": "b"}, priority=5, depends_on=[a.id])
        Worker(q, {"t": lambda p: order.append(p["n"])}).run_until_idle()
        self.assertEqual(order, ["a", "b"])
