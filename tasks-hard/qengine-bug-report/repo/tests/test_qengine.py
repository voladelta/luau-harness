import unittest

from qengine.catalog import Database
from qengine.evaluate import EvalError
from qengine.executor import execute
from qengine.format import render
from qengine.parser import ParseError


def sample():
    db = Database()
    db.create("emp", ["id", "name", "dept", "salary"], [
        (1, "ann", "eng", 120), (2, "bob", "eng", 100), (3, "cid", "ops", 90), (4, "dee", "ops", 95), (5, "eve", "hr", 70),
    ])
    db.create("dept", ["code", "title"], [("eng", "Engineering"), ("ops", "Operations"), ("fin", "Finance")])
    return db


class QengineTests(unittest.TestCase):
    def q(self, sql):
        return execute(sample(), sql).rows

    def test_select_where(self):
        self.assertEqual(self.q("SELECT name FROM emp WHERE salary >= 95 AND dept = 'ops'"), [("dee",)])

    def test_star(self):
        r = execute(sample(), "SELECT * FROM dept")
        self.assertEqual(r.columns, ["code", "title"])
        self.assertEqual(len(r.rows), 3)

    def test_arithmetic(self):
        self.assertEqual(self.q("SELECT salary * 2 + 1 AS x FROM emp WHERE id = 1"), [(241,)])

    def test_group_by(self):
        self.assertEqual(self.q("SELECT dept, COUNT(*), SUM(salary) FROM emp GROUP BY dept ORDER BY dept"),
                         [("eng", 2, 220), ("hr", 1, 70), ("ops", 2, 185)])

    def test_having(self):
        self.assertEqual(self.q("SELECT dept FROM emp GROUP BY dept HAVING COUNT(*) > 1 ORDER BY dept"), [("eng",), ("ops",)])

    def test_order_desc(self):
        self.assertEqual(self.q("SELECT name FROM emp ORDER BY salary DESC LIMIT 2"), [("ann",), ("bob",)])

    def test_inner_join(self):
        rows = self.q("SELECT e.name, d.title FROM emp e JOIN dept d ON e.dept = d.code ORDER BY e.id")
        self.assertEqual(rows[0], ("ann", "Engineering"))
        self.assertEqual(len(rows), 4)

    def test_left_join(self):
        rows = self.q("SELECT d.code, COUNT(e.id) FROM dept d LEFT JOIN emp e ON e.dept = d.code GROUP BY d.code ORDER BY d.code")
        self.assertEqual(rows[0], ("eng", 2))

    def test_distinct(self):
        self.assertEqual(self.q("SELECT DISTINCT dept FROM emp ORDER BY dept"), [("eng",), ("hr",), ("ops",)])

    def test_string_escape(self):
        self.assertEqual(self.q("SELECT 'it''s' AS s FROM dept LIMIT 1"), [("it's",)])

    def test_functions(self):
        self.assertEqual(self.q("SELECT UPPER(name), LENGTH(name) FROM emp WHERE id = 5"), [("EVE", 3)])

    def test_errors(self):
        with self.assertRaises(ParseError):
            execute(sample(), "SELECT FROM emp")
        with self.assertRaises(EvalError):
            execute(sample(), "SELECT nope FROM emp")

    def test_render(self):
        out = render(execute(sample(), "SELECT code FROM dept WHERE code = 'fin'"))
        self.assertEqual(out.splitlines()[-1], "(1 row)")


if __name__ == "__main__":
    unittest.main()
