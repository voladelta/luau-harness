import contextlib, io, json, os, tempfile, unittest

from logkit.cli import main
from logkit.parser import parse_line
from logkit.render import json_line

LOG = """2026-01-05T10:00:01Z INFO auth user=alice ms=120 "login ok"
2026-01-05T10:00:05Z WARN db ms=300 slow
2026-01-05T10:00:09Z ERROR db ms=900 "boom now"
2026-01-05T10:01:00Z INFO auth user=bob ms=80 "login ok"
"""


class T(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.TemporaryDirectory()
        self.p = os.path.join(self.d.name, "x.log")
        open(self.p, "w").write(LOG)

    def tearDown(self):
        self.d.cleanup()

    def run_cli(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = main([self.p, *args])
        return rc, out.getvalue(), err.getvalue()

    def test_json_line(self):
        d = json.loads(json_line(parse_line('2026-01-05T10:00:01Z INFO auth user=alice ms=120 "login ok"')))
        self.assertEqual(list(d), ["ts", "level", "component", "fields", "msg"])
        self.assertEqual(d, {"ts": "2026-01-05T10:00:01Z", "level": "INFO", "component": "auth", "fields": {"user": "alice", "ms": "120"}, "msg": "login ok"})

    def test_since_until(self):
        rc, out, _ = self.run_cli("--since", "2026-01-05T10:00:05Z", "--until", "2026-01-05T10:01:00Z")
        self.assertEqual(rc, 0)
        self.assertEqual([l.split()[1] for l in out.splitlines()], ["WARN", "ERROR"])
        rc, out, _ = self.run_cli("--until", "2026-01-05T10:00:05Z")
        self.assertEqual(len(out.splitlines()), 1)

    def test_combine_and_json(self):
        rc, out, _ = self.run_cli("--component", "db", "--since", "2026-01-05T10:00:06Z", "--json")
        self.assertEqual(rc, 0)
        lines = out.splitlines()
        self.assertEqual(len(lines), 1)
        self.assertEqual(json.loads(lines[0])["msg"], "boom now")
        rc, out, _ = self.run_cli("--json", "--summary")
        self.assertIn("records: 4", out)
        self.assertNotIn("{", out)

    def test_bad_ts(self):
        rc, out, err = self.run_cli("--since", "yesterday")
        self.assertEqual(rc, 2)
        self.assertTrue(err)
