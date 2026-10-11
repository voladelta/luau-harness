import unittest

from qengine.catalog import Database
from qengine.evaluate import EvalError
from qengine.executor import execute
from qengine.format import render


def db():
    d = Database()
    d.create("t", ["id", "name", "dept", "salary", "bonus"], [
        (1, "ann", "eng", 120, 10), (2, "bob", "eng", None, 5), (3, "cid", None, 90, None),
        (4, "dee", "ops", 95, None), (5, "eve", None, None, None), (6, None, "ops", 80, 0),
    ])
    d.create("d", ["code", "title"], [("eng", "Engineering"), ("ops", "Operations"), (None, "Nobody")])
    return d


def q(sql):
    return execute(db(), sql).rows


def ids(sql):
    return [r[0] for r in q(sql)]


class T(unittest.TestCase):
    def test_is_null(self):
        self.assertEqual(ids("SELECT id FROM t WHERE dept IS NULL ORDER BY id"), [3, 5])
        self.assertEqual(ids("SELECT id FROM t WHERE dept IS NOT NULL ORDER BY id"), [1, 2, 4, 6])
        self.assertEqual(ids("SELECT id FROM t WHERE NOT salary IS NULL ORDER BY id"), [1, 3, 4, 6])
        self.assertEqual(q("SELECT salary + 1 IS NULL AS x, bonus IS NOT NULL AS y FROM t WHERE id = 2"), [(True, True)])
        self.assertEqual(q("SELECT NULL IS NULL AS x FROM t WHERE id = 1"), [(True,)])

    def test_comparisons(self):
        self.assertEqual(ids("SELECT id FROM t WHERE salary > 85 ORDER BY id"), [1, 3, 4])
        self.assertEqual(ids("SELECT id FROM t WHERE NOT (salary > 85) ORDER BY id"), [6])
        self.assertEqual(ids("SELECT id FROM t WHERE salary = NULL"), [])
        self.assertEqual(ids("SELECT id FROM t WHERE salary != NULL"), [])
        self.assertEqual(ids("SELECT id FROM t WHERE NULL"), [])
        self.assertEqual(q("SELECT dept = 'eng' AS x FROM t ORDER BY id"), [(True,), (True,), (None,), (False,), (None,), (False,)])

    def test_arithmetic(self):
        self.assertEqual(q("SELECT salary + bonus AS s FROM t ORDER BY id"), [(130,), (None,), (None,), (None,), (None,), (80,)])
        self.assertEqual(q("SELECT -salary AS m, salary * 2 AS d, salary % 7 AS r FROM t WHERE id = 2"), [(None, None, None)])
        self.assertEqual(q("SELECT bonus / 0 AS z FROM t WHERE id = 3"), [(None,)])
        with self.assertRaises(EvalError):
            q("SELECT salary / 0 AS z FROM t WHERE id = 1")

    def test_three_valued_logic(self):
        self.assertEqual(ids("SELECT id FROM t WHERE salary > 100 OR bonus > 1 ORDER BY id"), [1, 2])
        self.assertEqual(ids("SELECT id FROM t WHERE NOT (salary > 100 AND bonus < 1) ORDER BY id"), [1, 2, 3, 4, 6])
        self.assertEqual(q("SELECT NULL AND FALSE, NULL OR TRUE, NULL AND TRUE, NULL OR FALSE, NOT NULL FROM t WHERE id = 1"),
                         [(False, True, None, None, None)])
        self.assertEqual(q("SELECT FALSE AND NULL, TRUE OR NULL, TRUE AND NULL FROM t WHERE id = 1"), [(False, True, None)])

    def test_functions(self):
        self.assertEqual(q("SELECT UPPER(name), LOWER(name), LENGTH(name) FROM t WHERE id = 6"), [(None, None, None)])
        self.assertEqual(q("SELECT ABS(salary), ROUND(salary, 0) FROM t WHERE id = 2"), [(None, None)])
        self.assertEqual(q("SELECT ROUND(90.4, bonus) FROM t WHERE id = 3"), [(None,)])
        self.assertEqual(q("SELECT COALESCE(dept, 'none'), COALESCE(NULL, NULL) FROM t WHERE id = 3"), [("none", None)])

    def test_aggregates(self):
        self.assertEqual(q("SELECT COUNT(*), COUNT(salary), COUNT(dept), SUM(bonus), AVG(salary), MIN(bonus), MAX(salary) FROM t"),
                         [(6, 4, 4, 15, 96.25, 0, 120)])
        self.assertEqual(q("SELECT COUNT(DISTINCT dept), COUNT(DISTINCT bonus), SUM(DISTINCT bonus) FROM t"), [(2, 3, 15)])

    def test_aggregates_without_values(self):
        self.assertEqual(q("SELECT COUNT(*), COUNT(bonus), SUM(bonus), AVG(bonus), MIN(bonus), MAX(bonus) FROM t WHERE dept IS NULL"),
                         [(2, 0, None, None, None, None)])
        self.assertEqual(q("SELECT COUNT(*), COUNT(salary), SUM(salary), AVG(salary), MIN(name) FROM t WHERE id > 100"),
                         [(0, 0, None, None, None)])

    def test_group_by_null(self):
        self.assertEqual(q("SELECT dept, COUNT(*), MAX(salary) FROM t GROUP BY dept ORDER BY dept"),
                         [("eng", 2, 120), ("ops", 2, 95), (None, 2, 90)])
        self.assertEqual(q("SELECT DISTINCT dept FROM t ORDER BY dept DESC"), [(None,), ("ops",), ("eng",)])

    def test_order_by_nulls(self):
        self.assertEqual(ids("SELECT id FROM t ORDER BY salary"), [6, 3, 4, 1, 2, 5])
        self.assertEqual(ids("SELECT id FROM t ORDER BY salary DESC"), [2, 5, 1, 4, 3, 6])
        self.assertEqual(ids("SELECT id FROM t ORDER BY dept DESC, id"), [3, 5, 4, 6, 1, 2])
        self.assertEqual(ids("SELECT id FROM t ORDER BY bonus, salary DESC"), [6, 2, 1, 5, 4, 3])

    def test_having(self):
        self.assertEqual(q("SELECT dept FROM t GROUP BY dept HAVING SUM(bonus) > 1"), [("eng",)])
        self.assertEqual(q("SELECT dept FROM t GROUP BY dept HAVING NOT (MAX(bonus) > 1) ORDER BY dept"), [("ops",)])

    def test_joins(self):
        self.assertEqual(q("SELECT t.id, d.title FROM t LEFT JOIN d ON t.dept = d.code ORDER BY t.id"),
                         [(1, "Engineering"), (2, "Engineering"), (3, None), (4, "Operations"), (5, None), (6, "Operations")])
        self.assertEqual(ids("SELECT t.id FROM t JOIN d ON t.dept = d.code ORDER BY t.id"), [1, 2, 4, 6])

    def test_render(self):
        out = render(execute(db(), "SELECT id, dept FROM t WHERE id = 3"))
        self.assertEqual(out.splitlines()[2].split(), ["3", "|", "NULL"])
