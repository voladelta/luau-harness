import unittest

from taskq.clock import FakeClock
from taskq.queue import JobQueue
from taskq.worker import Worker


def drain(q, w, c):
    for _ in range(30):
        w.run_until_idle()
        c.advance(100)


class T(unittest.TestCase):
    def test_runs_exactly_max_attempts(self):
        c = FakeClock()
        q = JobQueue(c)
        job = q.enqueue("boom", max_attempts=3)
        calls = []

        def boom(payload):
            calls.append(1)
            raise RuntimeError("x")

        drain(q, Worker(q, {"boom": boom}), c)
        self.assertEqual(len(calls), 3)
        self.assertEqual((job.status, job.attempts), ("dead", 3))

    def test_single_attempt_job(self):
        c = FakeClock()
        q = JobQueue(c)
        job = q.enqueue("boom", max_attempts=1)
        drain(q, Worker(q, {"boom": lambda p: 1 / 0}), c)
        self.assertEqual((job.status, job.attempts), ("dead", 1))

    def test_succeeds_on_retry(self):
        c = FakeClock()
        q = JobQueue(c)
        state = {"n": 0}

        def flaky(p):
            state["n"] += 1
            if state["n"] < 3:
                raise RuntimeError("again")

        job = q.enqueue("flaky", max_attempts=3)
        drain(q, Worker(q, {"flaky": flaky}), c)
        self.assertEqual((job.status, job.attempts), ("done", 3))
