import unittest

from logkit.parser import parse_line
from logkit.stats import latency_summary, percentile


class T(unittest.TestCase):
    def test_percentile(self):
        self.assertEqual(percentile(list(range(1, 21)), 95), 19)
        self.assertEqual(percentile([4, 1, 3, 2], 50), 2)
        self.assertEqual(percentile([5], 99), 5)
        self.assertEqual(percentile(list(range(1, 11)), 100), 10)
        self.assertEqual(percentile(list(range(1, 11)), 10), 1)

    def test_summary(self):
        recs = [parse_line(f"2026-01-05T10:00:{i:02d}Z INFO api ms={i}") for i in range(1, 21)]
        s = latency_summary(recs)
        self.assertEqual((s["count"], s["p50"], s["p95"], s["max"]), (20, 10.0, 19.0, 20.0))
