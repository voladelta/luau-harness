import unittest

from diffy.apply import PatchError, apply, reverse
from diffy.lcs import diff
from diffy.parse import ParseError, parse_patch
from diffy.unified import unified

A = ["one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"]


class DiffyTests(unittest.TestCase):
    def test_diff_ops(self):
        self.assertEqual(diff(["a", "b", "c"], ["a", "x", "c"]), [(" ", "a"), ("-", "b"), ("+", "x"), (" ", "c")])

    def test_unified_single_change(self):
        b = A[:4] + ["FIVE"] + A[5:]
        self.assertEqual(unified(A, b), "--- a\n+++ b\n@@ -2,7 +2,7 @@\n two\n three\n four\n-five\n+FIVE\n six\n seven\n eight\n")

    def test_identical(self):
        self.assertEqual(unified(A, list(A)), "")

    def test_two_hunks(self):
        b = ["ONE"] + A[1:9] + ["TEN"]
        out = unified(A, b, context=1)
        self.assertEqual(out.count("@@ -"), 2)

    def test_roundtrip(self):
        b = ["zero"] + A[:2] + A[3:7] + ["7.5"] + A[7:]
        p = parse_patch(unified(A, b))
        self.assertEqual(apply(p, A), b)
        self.assertEqual(apply(reverse(p), b), A)

    def test_apply_mismatch(self):
        p = parse_patch(unified(A, A[:4] + ["FIVE"] + A[5:]))
        with self.assertRaises(PatchError) as cm:
            apply(p, ["x"] + A)
        self.assertEqual(cm.exception.index, 1)

    def test_parse_errors(self):
        with self.assertRaises(ParseError):
            parse_patch("no header here\n")
        with self.assertRaises(ParseError):
            parse_patch("--- a\n+++ b\n@@ -1,2 +1,2 @@\n x\n")


if __name__ == "__main__":
    unittest.main()
