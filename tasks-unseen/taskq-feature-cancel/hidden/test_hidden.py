import json, unittest

from taskq.clock import FakeClock
from taskq.queue import JobQueue
from taskq.store import dump, load
from taskq.worker import Worker


class T(unittest.TestCase):
    def test_cancel_queued(self):
        c = FakeClock(100.0)
        q = JobQueue(c)
        a, b = q.enqueue("a", priority=5), q.enqueue("b")
        self.assertIsNone(a.cancelled_at)
        self.assertIs(q.cancel(a.id), a)
        self.assertEqual((a.status, a.cancelled_at), ("cancelled", 100.0))
        self.assertEqual(q.pop_ready().id, b.id)
        self.assertIsNone(q.pop_ready())
        c.advance(5)
        self.assertIs(q.cancel(a.id), a)
        self.assertEqual(a.cancelled_at, 100.0)
        self.assertEqual(q.stats(), {"cancelled": 1, "running": 1})

    def test_cannot_cancel_other_states(self):
        q = JobQueue(FakeClock())
        a = q.enqueue("a", max_attempts=1)
        q.pop_ready()
        with self.assertRaisesRegex(ValueError, f"cannot cancel running job: {a.id}"):
            q.cancel(a.id)
        q.fail(a, "x")
        with self.assertRaisesRegex(ValueError, f"cannot cancel dead job: {a.id}"):
            q.cancel(a.id)
        b = q.enqueue("b")
        q.complete(q.pop_ready())
        with self.assertRaisesRegex(ValueError, "cannot cancel done job"):
            q.cancel(b.id)
        with self.assertRaises(KeyError):
            q.cancel("job-99")

    def test_worker_skips_cancelled(self):
        q = JobQueue(FakeClock())
        seen = []
        a = q.enqueue("t", {"n": 1})
        q.enqueue("t", {"n": 2})
        q.cancel(a.id)
        Worker(q, {"t": lambda p: seen.append(p["n"])}).run_until_idle()
        self.assertEqual(seen, [2])

    def test_store(self):
        c = FakeClock(7.0)
        q = JobQueue(c)
        a = q.enqueue("a")
        q.cancel(a.id)
        q2 = load(c, dump(q))
        self.assertEqual((q2.jobs[a.id].status, q2.jobs[a.id].cancelled_at), ("cancelled", 7.0))
        old = json.loads(dump(q))
        for j in old["jobs"]:
            del j["cancelled_at"]
        self.assertIsNone(load(c, json.dumps(old)).jobs[a.id].cancelled_at)
