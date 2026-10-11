import os
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO

from diffy import cli
from diffy.apply import PatchError, apply, apply_fuzzy, report
from diffy.parse import parse_patch
from diffy.unified import unified

A = [f"L{i}" for i in range(1, 41)]
B = list(A)
B[4] = "CHANGED 5"           # hunk 1 (lines 2-8)
B[19:20] = ["NEW a", "NEW b"]  # hunk 2: L20 -> two lines
B.remove("L34")              # hunk 3: drop L34 (lines 31-37)
P = parse_patch(unified(A, B))

TWO = parse_patch("--- a\n+++ b\n@@ -1,3 +1,3 @@\n a1\n-a2\n+A2\n a3\n@@ -11,3 +11,3 @@\n x\n-y\n+Y\n z\n")


def summary(res):
    return [(h.status, h.line, h.offset, h.fuzz) for h in res.hunks]


class T(unittest.TestCase):
    def test_clean(self):
        res = apply_fuzzy(P, A)
        self.assertEqual(res.lines, B)
        self.assertEqual(summary(res), [("applied", 2, 0, 0), ("applied", 17, 0, 0), ("applied", 31, 0, 0)])
        self.assertEqual(report(res), [])

    def test_offsets(self):
        res = apply_fuzzy(P, ["top1", "top2", "top3"] + A)
        self.assertEqual(res.lines, ["top1", "top2", "top3"] + B)
        self.assertEqual(summary(res), [("applied", 5, 3, 0), ("applied", 20, 3, 0), ("applied", 34, 3, 0)])
        self.assertEqual(report(res), ["Hunk #1 succeeded at 5 (offset 3 lines).", "Hunk #2 succeeded at 20 (offset 3 lines).",
                                       "Hunk #3 succeeded at 34 (offset 3 lines)."])
        res = apply_fuzzy(P, A[1:])
        self.assertEqual(res.lines, B[1:])
        self.assertEqual(report(res)[0], "Hunk #1 succeeded at 1 (offset -1 line).")

    def test_mixed_offsets(self):
        drift = lambda xs: [y for x in xs if x not in ("L29", "L30") for y in ([x] + ["extra"] * 4 if x == "L12" else [x])]
        res = apply_fuzzy(P, drift(A))  # +4 before hunk 2, then -2 before hunk 3
        self.assertEqual(summary(res), [("applied", 2, 0, 0), ("applied", 21, 4, 0), ("applied", 33, 2, 0)])
        self.assertEqual(res.lines, drift(B))

    def test_expected_position_follows_last_offset(self):
        f = [f"p{i}" for i in range(10)] + ["a1", "a2", "a3", "q", "q"] + ["x", "y", "z"] + ["r", "r"] + ["x", "y", "z"]
        res = apply_fuzzy(TWO, f)
        self.assertEqual(summary(res), [("applied", 11, 10, 0), ("applied", 21, 10, 0)])
        self.assertEqual(res.lines[15:18], ["x", "y", "z"])
        self.assertEqual(res.lines[20:23], ["x", "Y", "z"])

    def test_tie_prefers_later(self):
        p = parse_patch("--- a\n+++ b\n@@ -6,3 +6,3 @@\n x\n-y\n+Y\n z\n")
        f = ["f0", "f1", "f2", "x", "y", "z", "f6", "x", "y", "z", "f10"]
        res = apply_fuzzy(p, f)
        self.assertEqual(summary(res), [("applied", 8, 2, 0)])
        self.assertEqual(res.lines[7:10], ["x", "Y", "z"])

    def test_no_overlap_with_previous_hunk(self):
        f = [f"p{i}" for i in range(10)] + ["a1", "a2", "a3"]
        g = ["x", "y", "z"] + f[3:]  # hunk 2's only match lies before where hunk 1 lands
        res = apply_fuzzy(TWO, g)
        self.assertEqual(summary(res), [("applied", 11, 10, 0), ("failed", 21, 0, 0)])
        self.assertEqual(res.lines, ["x", "y", "z"] + f[3:10] + ["a1", "A2", "a3"])
        self.assertEqual(report(res), ["Hunk #1 succeeded at 11 (offset 10 lines).", "Hunk #2 FAILED at 21."])

    def test_failed_hunk_does_not_move_others(self):
        f = list(A)
        f[4] = "someone else changed this"
        f[5] = "and this"
        f = ["new top"] + f
        res = apply_fuzzy(P, f, fuzz=1)
        self.assertEqual(summary(res), [("failed", 2, 0, 0), ("applied", 18, 1, 0), ("applied", 32, 1, 0)])
        self.assertEqual(res.lines[:7], f[:7])

    def test_fuzz_one(self):
        f = list(A)
        f[1] = "L2 edited"  # first context line of hunk 1
        res = apply_fuzzy(P, f)
        self.assertEqual(summary(res)[0], ("applied", 2, 0, 1))
        self.assertEqual(res.lines[1], "L2 edited")
        self.assertEqual(res.lines[4], "CHANGED 5")
        self.assertEqual(report(res), ["Hunk #1 succeeded at 2 with fuzz 1."])
        self.assertEqual(summary(apply_fuzzy(P, f, fuzz=0))[0], ("failed", 2, 0, 0))

    def test_fuzz_with_offset(self):
        f = list(A)
        f[36] = "L37 edited"  # last context line of hunk 3
        f = f[:10] + f[12:]
        res = apply_fuzzy(P, f)
        self.assertEqual(summary(res)[2], ("applied", 29, -2, 1))
        self.assertEqual(report(res)[-1], "Hunk #3 succeeded at 29 with fuzz 1 (offset -2 lines).")
        self.assertNotIn("L34", res.lines)
        self.assertIn("L37 edited", res.lines)

    def test_fuzz_two(self):
        f = list(A)
        f[1], f[2] = "L2 edited", "L3 edited"
        self.assertEqual(summary(apply_fuzzy(P, f, fuzz=1))[0][0], "failed")
        res = apply_fuzzy(P, f)
        self.assertEqual(summary(res)[0], ("applied", 2, 0, 2))
        self.assertEqual(res.lines[:5], ["L1", "L2 edited", "L3 edited", "L4", "CHANGED 5"])

    def test_exact_far_beats_fuzzy_near(self):
        p = parse_patch("--- a\n+++ b\n@@ -2,5 +2,5 @@\n c1\n c2\n-old\n+new\n c3\n c4\n")
        f = ["s", "c1-edited", "c2", "old", "c3", "c4"] + ["pad"] * 8 + ["c1", "c2", "old", "c3", "c4"]
        res = apply_fuzzy(p, f)
        self.assertEqual(summary(res), [("applied", 15, 13, 0)])
        self.assertEqual(res.lines[3], "old")
        self.assertEqual(res.lines[16], "new")

    def test_one_sided_context(self):
        p = parse_patch("--- a\n+++ b\n@@ -1,3 +1,3 @@\n-first\n+FIRST\n c1\n c2\n")
        res = apply_fuzzy(p, ["first", "c1", "c2 edited", "more"])
        self.assertEqual(summary(res), [("applied", 1, 0, 1)])
        self.assertEqual(res.lines, ["FIRST", "c1", "c2 edited", "more"])
        self.assertEqual(summary(apply_fuzzy(p, ["first", "c1 edited", "c2 edited"]))[0][0], "applied")
        self.assertEqual(summary(apply_fuzzy(p, ["first", "c1 edited", "c2 edited"], fuzz=1))[0][0], "failed")

    def test_pure_insertion(self):
        p = parse_patch("--- a\n+++ b\n@@ -3,0 +4 @@\n+new\n")
        self.assertEqual(apply_fuzzy(p, ["a", "b", "c", "d"]).lines, ["a", "b", "c", "new", "d"])
        res = apply_fuzzy(p, ["a", "b"])
        self.assertEqual(res.lines, ["a", "b", "new"])
        self.assertEqual(summary(res), [("applied", 3, -1, 0)])
        self.assertEqual(report(res), ["Hunk #1 succeeded at 3 (offset -1 line)."])

    def test_errors_and_strict_apply(self):
        with self.assertRaises(ValueError):
            apply_fuzzy(P, A, fuzz=-1)
        with self.assertRaises(PatchError):
            apply(P, ["top"] + A)
        self.assertEqual(apply(P, A), B)

    def test_cli(self):
        with tempfile.TemporaryDirectory() as d:
            pf, tf = os.path.join(d, "x.patch"), os.path.join(d, "f.txt")
            with open(pf, "w") as f:
                f.write("--- a\n+++ b\n@@ -1,3 +1,3 @@\n a1\n-a2\n+A2\n a3\n@@ -11,3 +11,3 @@\n x\n-y\n+Y\n z\n")
            with open(tf, "w") as f:
                f.write("".join(l + "\n" for l in ["zz", "a1", "a2", "a3", "b"]))
            out = StringIO()
            with redirect_stdout(out):
                code = cli.main(["apply", pf, tf])
            self.assertEqual(code, 1)
            self.assertEqual(out.getvalue(), "Hunk #1 succeeded at 2 (offset 1 line).\nHunk #2 FAILED at 12.\n")
            with open(tf) as f:
                self.assertEqual(f.read(), "zz\na1\nA2\na3\nb\n")
            with open(tf + ".rej") as f:
                self.assertEqual(f.read(), "--- a\n+++ b\n@@ -11,3 +11,3 @@\n x\n-y\n+Y\n z\n")

    def test_cli_fuzz_flag(self):
        with tempfile.TemporaryDirectory() as d:
            pf, tf = os.path.join(d, "x.patch"), os.path.join(d, "f.txt")
            with open(pf, "w") as f:
                f.write("--- a\n+++ b\n@@ -1,3 +1,3 @@\n a1\n-a2\n+A2\n a3\n")
            for flag, code_want, body in ((["--fuzz", "0"], 1, "a1 edited\na2\na3\n"), ([], 0, "a1 edited\nA2\na3\n")):
                with open(tf, "w") as f:
                    f.write("a1 edited\na2\na3\n")
                out = StringIO()
                with redirect_stdout(out):
                    code = cli.main(["apply", pf, tf] + flag)
                self.assertEqual(code, code_want)
                with open(tf) as f:
                    self.assertEqual(f.read(), body)
                self.assertEqual(os.path.exists(tf + ".rej"), code_want == 1)
                if os.path.exists(tf + ".rej"):
                    os.remove(tf + ".rej")
            self.assertEqual(out.getvalue(), "Hunk #1 succeeded at 1 with fuzz 1.\n")
