import unittest

from logkit.filters import between, by_level
from logkit.parser import parse_line
from logkit.stats import percentile
from logkit.timeutil import parse_ts

LINE = '2026-01-05T10:00:03Z INFO auth user=alice ms=120 "login ok"'


class LogkitTests(unittest.TestCase):
    def test_parse(self):
        r = parse_line(LINE)
        self.assertEqual((r["level"], r["component"], r["fields"]["ms"], r["msg"]), ("INFO", "auth", "120", "login ok"))
        self.assertIsNone(parse_line("garbage"))

    def test_filters(self):
        rs = [parse_line(LINE), parse_line("2026-01-05T10:00:09Z ERROR db ms=900 boom")]
        self.assertEqual(len(by_level(rs, "WARN")), 1)
        self.assertEqual(len(between(rs, start=parse_ts("2026-01-05T10:00:05Z"))), 1)

    def test_percentile(self):
        self.assertEqual(percentile([1, 2, 3, 4, 5], 50), 3)


if __name__ == "__main__":
    unittest.main()
