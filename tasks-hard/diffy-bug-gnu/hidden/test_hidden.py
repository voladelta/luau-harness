import difflib
import random
import unittest

from diffy.apply import apply, reverse
from diffy.parse import parse_patch
from diffy.unified import unified

A = [f"line {i}" for i in range(1, 21)]


def gnu(a, b, n=3):
    return "".join(l + "\n" for l in difflib.unified_diff(a, b, "a", "b", n=n, lineterm=""))


class T(unittest.TestCase):
    def test_insert_only_header(self):
        self.assertEqual(unified(["a", "b"], ["a", "x", "b"], context=0), "--- a\n+++ b\n@@ -1,0 +2 @@\n+x\n")
        self.assertEqual(unified([], ["x", "y", "z"]), "--- a\n+++ b\n@@ -0,0 +1,3 @@\n+x\n+y\n+z\n")
        self.assertEqual(unified(["a"], ["x", "y", "a"], context=0), "--- a\n+++ b\n@@ -0,0 +1,2 @@\n+x\n+y\n")

    def test_delete_only_header(self):
        self.assertEqual(unified(["a", "b", "c"], ["a", "c"], context=0), "--- a\n+++ b\n@@ -2 +1,0 @@\n-b\n")
        self.assertEqual(unified(["x", "y"], []), "--- a\n+++ b\n@@ -1,2 +0,0 @@\n-x\n-y\n")

    def test_gap_of_exactly_twice_context(self):
        b = list(A)
        b[3], b[10] = "CHANGED 4", "CHANGED 11"  # 6 unchanged lines between
        out = unified(A, b)
        self.assertEqual(out.count("\n@@ "), 1)
        self.assertEqual(out, gnu(A, b))
        b2 = list(A)
        b2[3], b2[11] = "CHANGED 4", "CHANGED 12"  # 7 unchanged lines: two hunks
        self.assertEqual(unified(A, b2).count("\n@@ "), 2)
        self.assertEqual(unified(A, b2), gnu(A, b2))
        b3 = list(A)
        b3[2], b3[5] = "x", "y"
        self.assertEqual(unified(A, b3, context=1), gnu(A, b3, 1))

    def test_apply_gnu_patches(self):
        self.assertEqual(apply(parse_patch("--- a\n+++ b\n@@ -0,0 +1,3 @@\n+x\n+y\n+z\n"), []), ["x", "y", "z"])
        self.assertEqual(apply(parse_patch("--- a\n+++ b\n@@ -1,0 +2 @@\n+x\n"), ["a", "b"]), ["a", "x", "b"])
        self.assertEqual(apply(parse_patch("--- a\n+++ b\n@@ -2 +1,0 @@\n-b\n"), ["a", "b", "c"]), ["a", "c"])
        self.assertEqual(apply(parse_patch("--- a\n+++ b\n@@ -0,0 +1 @@\n+top\n@@ -3,0 +5,2 @@\n+p\n+q\n"), ["a", "b", "c", "d"]),
                         ["top", "a", "b", "c", "p", "q", "d"])

    def test_random_against_difflib(self):
        r = random.Random(42)
        for _ in range(400):
            a = [f"l{i}" for i in range(r.randint(0, 25))]
            b = []
            for x in a:
                p = r.random()
                if p < 0.12:
                    continue
                if p < 0.2:
                    b.append(f"n{r.randint(0, 99)}")
                b.append(x)
            n = r.randint(0, 4)
            want = gnu(a, b, n)
            self.assertEqual(unified(a, b, context=n), want)
            if want:
                p = parse_patch(want)
                self.assertEqual(apply(p, a), b)
                self.assertEqual(apply(reverse(p), b), a)
