import contextlib, io, os, tempfile, unittest

from logkit.cli import main
from logkit.parser import parse_line
from logkit.stats import top_n

LINES = ["2026-01-05T10:00:0%dZ %s %s ms=1" % (i, lvl, comp) for i, (lvl, comp) in enumerate(
    [("INFO", "db"), ("INFO", "auth"), ("ERROR", "db"), ("WARN", "api"), ("INFO", "api"), ("INFO", "auth"), ("INFO", "db")])]


class T(unittest.TestCase):
    def test_top_n(self):
        rs = [parse_line(l) for l in LINES]
        self.assertEqual(top_n(rs, "component", 2), [("db", 3), ("api", 2)])
        self.assertEqual(top_n(rs, "component", 10), [("db", 3), ("api", 2), ("auth", 2)])
        self.assertEqual(top_n(rs, "level", 1), [("INFO", 5)])
        self.assertEqual(top_n(rs, "level", 0), [])
        self.assertEqual(top_n([], "level", 3), [])

    def run_cli(self, *args):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "x.log")
            open(p, "w").write("\n".join(LINES) + "\n")
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = main([p, *args])
            return rc, out.getvalue(), err.getvalue()

    def test_cli(self):
        rc, out, _ = self.run_cli("--top", "component", "2")
        self.assertEqual((rc, out), (0, "db 3\napi 2\n"))
        rc, out, _ = self.run_cli("--level", "WARN", "--top", "component", "5")
        self.assertEqual(out, "api 1\ndb 1\n")
        for bad in (("--top", "ms", "2"), ("--top", "level", "x"), ("--top", "level")):
            rc, out, err = self.run_cli(*bad)
            self.assertEqual(rc, 2)
            self.assertTrue(err)
