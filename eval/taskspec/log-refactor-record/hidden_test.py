import contextlib, dataclasses, io, os, tempfile, unittest
from datetime import datetime, timezone

from logkit.cli import main
from logkit.filters import between, by_component, by_level, has_field
from logkit.parser import parse_line, read_records
from logkit.record import Record
from logkit.render import summary_text
from logkit.stats import count_by, error_rate, latency_summary

L1 = '2026-01-05T10:00:01Z INFO auth user=alice ms=120 "login ok"'
L2 = "2026-01-05T10:00:05Z ERROR db ms=900 boom"


class T(unittest.TestCase):
    def test_record_type(self):
        self.assertTrue(dataclasses.is_dataclass(Record))
        r = parse_line(L1)
        self.assertIsInstance(r, Record)
        self.assertEqual((r.level, r.component, r.fields, r.msg, r.raw), ("INFO", "auth", {"user": "alice", "ms": "120"}, "login ok", L1))
        self.assertEqual(r.ts, datetime(2026, 1, 5, 10, 0, 1, tzinfo=timezone.utc))
        self.assertIsNone(parse_line("junk"))
        self.assertEqual(r, Record(r.ts, "INFO", "auth", {"user": "alice", "ms": "120"}, "login ok", L1))

    def test_filters_and_stats(self):
        rs = [parse_line(L1), parse_line(L2)]
        self.assertEqual([r.level for r in by_level(rs, "WARN")], ["ERROR"])
        self.assertEqual(len(by_component(rs, "auth")), 1)
        self.assertEqual(len(between(rs, start=rs[1].ts)), 1)
        self.assertEqual(len(between(rs, end=rs[1].ts)), 1)
        self.assertEqual(len(has_field(rs, "user")), 1)
        self.assertEqual(count_by(rs, "level"), {"INFO": 1, "ERROR": 1})
        self.assertEqual(count_by(rs, "component"), {"auth": 1, "db": 1})
        self.assertEqual(error_rate(rs), 0.5)
        s = latency_summary(rs)
        self.assertEqual((s["count"], s["p50"], s["max"]), (2, 120.0, 900.0))
        self.assertIn("records: 2", summary_text(rs))

    def test_cli_and_read(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "x.log")
            open(p, "w").write(L1 + "\nnot a log line\n" + L2 + "\n")
            self.assertTrue(all(isinstance(r, Record) for r in read_records(p)))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(main([p, "--level", "ERROR"]), 0)
            self.assertEqual(out.getvalue(), L2 + "\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                main([p, "--summary"])
            self.assertIn("error rate: 50.0%", out.getvalue())
