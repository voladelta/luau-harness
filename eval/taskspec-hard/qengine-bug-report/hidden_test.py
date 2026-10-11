import unittest

from qengine.catalog import Database
from qengine.evaluate import EvalError
from qengine.executor import execute


def db():
    d = Database()
    d.create("staff", ["id", "name", "dept", "salary", "bonus"],
             [(i, f"n{(i * 7) % 25:02d}", ["ops", "eng", "hr"][i % 3], 100 + (i * 13) % 50, i % 4) for i in range(1, 26)])
    d.create("dept", ["code", "title"], [("eng", "E"), ("fin", "F"), ("ops", "O"), ("law", "L"), ("hr", "H"), ("art", "A")])
    return d


def q(sql):
    return execute(db(), sql).rows


class T(unittest.TestCase):
    def test_subtraction_is_left_associative(self):
        self.assertEqual(q("SELECT 10 - 3 - 2 AS x FROM staff LIMIT 1"), [(5,)])
        self.assertEqual(q("SELECT salary - bonus + 100 AS x FROM staff WHERE id = 3"), [(100 + 39 - 3 + 100,)])
        self.assertEqual(q("SELECT 2 * 3 - 4 - 1 + 6 / 2 AS x FROM staff LIMIT 1"), [(4.0,)])
        self.assertEqual(q("SELECT 1 - -2 - 3 AS x FROM staff LIMIT 1"), [(0,)])
        self.assertEqual(q("SELECT 100 / 10 / 5 AS x, 7 - 2 * 3 AS y FROM staff LIMIT 1"), [(2.0, 1)])

    def test_order_by_position(self):
        self.assertEqual(q("SELECT name, id FROM staff WHERE id <= 4 ORDER BY 1"), [("n03", 4), ("n07", 1), ("n14", 2), ("n21", 3)])
        self.assertEqual([r[1] for r in q("SELECT name, id FROM staff WHERE id <= 4 ORDER BY 2 DESC")], [4, 3, 2, 1])
        with self.assertRaises(EvalError):
            q("SELECT name, id FROM staff ORDER BY 3")
        with self.assertRaises(EvalError):
            q("SELECT name, id FROM staff ORDER BY 0")

    def test_multi_key_order(self):
        rows = q("SELECT dept, name FROM staff WHERE id <= 9 ORDER BY dept, name")
        self.assertEqual(rows, sorted(rows))
        self.assertEqual([r[0] for r in rows], ["eng"] * 3 + ["hr"] * 3 + ["ops"] * 3)
        rows = q("SELECT dept, salary, id FROM staff ORDER BY dept DESC, salary, id DESC")
        self.assertEqual(rows, sorted(rows, key=lambda r: (-ord(r[0][0]), r[1], -r[2])))

    def test_limit_offset(self):
        page = lambda off: [r[0] for r in q(f"SELECT id FROM staff ORDER BY id LIMIT 10 OFFSET {off}")]
        self.assertEqual(page(0), list(range(1, 11)))
        self.assertEqual(page(10), list(range(11, 21)))
        self.assertEqual(page(20), list(range(21, 26)))
        self.assertEqual([r[0] for r in q("SELECT id FROM staff ORDER BY id OFFSET 23")], [24, 25])
        self.assertEqual([r[0] for r in q("SELECT id FROM staff ORDER BY id LIMIT 2 OFFSET 1")], [2, 3])

    def test_left_join_keeps_unmatched(self):
        rows = q("SELECT d.code, e.name FROM dept d LEFT JOIN staff e ON e.dept = d.code")
        self.assertEqual([r for r in rows if r[1] is None], [("fin", None), ("law", None), ("art", None)])
        self.assertEqual(len(rows), 25 + 3)
        self.assertEqual(q("SELECT d.code, COUNT(*) AS n FROM dept d LEFT JOIN staff e ON e.dept = d.code GROUP BY d.code ORDER BY n DESC, d.code"),
                         [("eng", 9), ("hr", 8), ("ops", 8), ("art", 1), ("fin", 1), ("law", 1)])

    def test_unchanged_behaviour(self):
        self.assertEqual(q("SELECT dept, COUNT(*) FROM staff GROUP BY dept ORDER BY dept"), [("eng", 9), ("hr", 8), ("ops", 8)])
        self.assertEqual(q("SELECT e.id FROM staff e JOIN dept d ON e.dept = d.code WHERE d.code = 'hr' ORDER BY e.id LIMIT 3"), [(2,), (5,), (8,)])
        self.assertEqual(q("SELECT name AS n FROM staff WHERE id < 4 ORDER BY n DESC"), [("n21",), ("n14",), ("n07",)])
