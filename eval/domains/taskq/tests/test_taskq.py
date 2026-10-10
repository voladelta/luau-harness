import unittest

from taskq.clock import FakeClock
from taskq.queue import JobQueue
from taskq.retry import backoff
from taskq.store import dump, load
from taskq.worker import Worker


class TaskqTests(unittest.TestCase):
    def test_priority_order(self):
        q = JobQueue(FakeClock())
        q.enqueue("a", priority=1)
        hi = q.enqueue("b", priority=5)
        self.assertEqual(q.pop_ready().id, hi.id)

    def test_delay(self):
        c = FakeClock()
        q = JobQueue(c)
        q.enqueue("a", delay=10)
        self.assertIsNone(q.pop_ready())
        c.advance(10)
        self.assertIsNotNone(q.pop_ready())

    def test_worker_and_roundtrip(self):
        c = FakeClock()
        q = JobQueue(c)
        q.enqueue("ok", {"x": 1})
        seen = []
        Worker(q, {"ok": seen.append}).run_until_idle()
        self.assertEqual(seen, [{"x": 1}])
        self.assertEqual(load(c, dump(q)).stats(), {"done": 1})
        self.assertEqual(backoff(1), 2.0)


if __name__ == "__main__":
    unittest.main()
