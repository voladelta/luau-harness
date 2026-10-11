import hashlib, signal, time, unittest

from qengine.catalog import Database
from qengine.executor import execute

import random


def make_db(Database):
    r = random.Random(7)
    keys = list(range(12)) + [3.0, 5.0, True, None, "3", "x"]
    d = Database()
    d.create("a", ["id", "k", "v", "s"], [(i, r.choice(keys), r.randint(0, 9), r.choice("xyz")) for i in range(1, 121)])
    d.create("b", ["id", "k", "w"], [(i, r.choice(keys), r.randint(0, 9)) for i in range(1, 101)])
    d.create("c", ["id", "k2", "z"], [(i, r.choice(keys), r.randint(0, 9)) for i in range(1, 61)])
    d.create("e", ["id", "k"], [])
    return d

EXPECTED = {  # sha256 of repr((columns, rows)) produced by the original nested-loop join
    'SELECT * FROM a JOIN b ON a.k = b.k': '2571fe6ea63ce03dfd55c4e7653e8558953e61557d4cebb11fb1620636d236bb',
    'SELECT a.id, b.id FROM a LEFT JOIN b ON b.k = a.k AND b.w > a.v': '8ceb5dccec08db95afa5912ba0c2be57d03a2bcbe82426014eb184bfd2e5903c',
    'SELECT a.id, b.id FROM a JOIN b ON a.v + 1 = b.w': 'ac4aa250965b4e064d16dfc975f2a8073c4289e1ea71c833381f39f8f540f8ea',
    'SELECT a.id, b.id FROM a JOIN b ON a.k = b.k OR a.v = b.w': 'a0d07abf5f1f71c7bf2776cb339986b82427e9e2b6f1bc81a5c1992b49bdedcf',
    'SELECT a.id, b.id, c.id FROM a JOIN b ON a.k = b.k LEFT JOIN c ON c.k2 = b.w AND c.z != a.v': 'a9244c0d83d81dc80c248abd8bd9050c2a0eacfcfcdbf5a187bf3072298ef75d',
    'SELECT a.id, b.id FROM a LEFT JOIN b ON a.v < b.w AND a.k = b.k': '8ceb5dccec08db95afa5912ba0c2be57d03a2bcbe82426014eb184bfd2e5903c',
    'SELECT a.id, c.id FROM a JOIN c ON k = k2': 'a2f3390ab7da5958e4a2d5d4a2d6dc2f0a6b8b79d0e49aefca6b5dcf9284e017',
    "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k AND a.s = 'x'": '435e7a9c116d8d0c154bc8d9ff75baf9b11335429ac624535b0b222b5af38a6b',
    'SELECT a.id, b.id FROM a JOIN b ON b.k = b.w': 'd619e5637f1d7ed313094630818a780032a358ec778ab27a1f395a1c6a2c9927',
    'SELECT x.id, y.id FROM a x JOIN b y ON y.k = x.k WHERE y.w > 4': 'b46804913a5ec4838f37063a5e4c72ecbab04968c3cfada966327ff0373f74c2',
    'SELECT x.id, y.id FROM a x JOIN a y ON x.k = y.k AND x.id < y.id': '3900ce9c48bc9d27ad5a1cacf13c9309950b96f64ceb60515bb5e4e72e40b7cb',
    'SELECT a.id, e.id FROM a LEFT JOIN e ON a.k = e.k': '881ced847fcd9780d92f2983b55c9979e761e1db02318617eaf2a11445394cae',
    'SELECT a.id, c.id FROM a LEFT JOIN c ON NOT (a.k = c.k2) AND c.z = a.v': '869e877e4aa3cff7b54bb0681e6ee73c6be13526f1acb366d1740bd350aefaf9',
    'SELECT a.s, COUNT(*), SUM(b.w) FROM a JOIN b ON a.k = b.k GROUP BY a.s ORDER BY a.s': 'b47b9030ca7e003a59aec7c5599dd985d92c84ac2878b7dad165580f8e61bc66',
    'SELECT a.id, b.id FROM a JOIN b ON (a.k = b.k AND a.v = b.w) AND b.id > 10': 'acc3aa529c827d8674bce31db124d99d2e2c844503865548b71e4deb8b6bd64d',
    'SELECT c.id, a.id FROM c LEFT JOIN a ON a.k = c.k2 AND a.v = c.z': '6e1976d7e64b1d0b54e47d6d2b28ec04455c53bb1cc5b00c151878045e24c59b',
}


def digest(r):
    return hashlib.sha256(repr((r.columns, r.rows)).encode()).hexdigest()


class Deadline:
    """Abort a runaway (quadratic) join instead of waiting minutes for it."""
    def __init__(self, secs): self.secs = secs
    def __enter__(self):
        def boom(*_): raise TimeoutError(f"join took longer than {self.secs}s")
        self.old = signal.signal(signal.SIGALRM, boom)
        signal.setitimer(signal.ITIMER_REAL, self.secs)
        self.t0 = time.perf_counter()
        return self
    def __exit__(self, *exc):
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, self.old)
        self.elapsed = time.perf_counter() - self.t0


def big(n):
    d = Database()
    d.create("orders", ["id", "cust", "amount"], [(i, i % (n // 2), i % 97) for i in range(n)])
    d.create("cust", ["cid", "name", "region"], [(i, f"c{i}", i % 7) for i in range(n)])
    d.create("region", ["rid", "rname"], [(i, f"r{i}") for i in range(n) if i % 2 == 0])
    return d


class T(unittest.TestCase):
    def test_same_results_as_nested_loop(self):
        for sql, want in EXPECTED.items():
            with self.subTest(sql=sql):
                self.assertEqual(digest(execute(make_db(Database), sql)), want)

    def test_small_explicit(self):
        d = Database()
        d.create("l", ["id", "k"], [(1, 1), (2, 2.0), (3, None), (4, "1"), (5, 9)])
        d.create("r", ["k", "tag"], [(1.0, "a"), (True, "b"), (2, "c"), (None, "d"), ("1", "e"), (2, "f")])
        rows = execute(d, "SELECT l.id, r.tag FROM l LEFT JOIN r ON l.k = r.k").rows
        self.assertEqual(rows, [(1, "a"), (1, "b"), (2, "c"), (2, "f"), (3, "d"), (4, "e"), (5, None)])
        rows = execute(d, "SELECT l.id, r.tag FROM l JOIN r ON r.k = l.k AND r.tag != 'b'").rows
        self.assertEqual(rows, [(1, "a"), (2, "c"), (2, "f"), (3, "d"), (4, "e")])

    def test_perf_inner(self):
        d = big(4000)
        with Deadline(6) as t:
            r = execute(d, "SELECT o.id, c.name FROM orders o JOIN cust c ON o.cust = c.cid AND o.amount >= 0")
        self.assertEqual(len(r.rows), 4000)
        self.assertEqual(r.rows[:2], [(0, "c0"), (1, "c1")])
        self.assertLess(t.elapsed, 2.0)

    def test_perf_chain_left(self):
        d = big(4000)
        with Deadline(8) as t:
            r = execute(d, "SELECT c.cid, o.id, g.rname FROM cust c LEFT JOIN orders o ON c.cid = o.cust "
                           "LEFT JOIN region g ON g.rid = c.region * 2 WHERE c.cid < 2500")
        self.assertEqual(len(r.rows), 2000 * 2 + 500)
        self.assertEqual(r.rows[:3], [(0, 0, "r0"), (0, 2000, "r0"), (1, 1, "r2")])
        self.assertEqual(r.rows[-1], (2499, None, "r0"))
        self.assertLess(t.elapsed, 2.5)

    def test_perf_unqualified_swapped(self):
        d = big(4000)
        with Deadline(6) as t:
            r = execute(d, "SELECT id, name FROM orders JOIN cust ON cid = cust WHERE amount = 3")
        self.assertEqual([x[0] for x in r.rows], [i for i in range(4000) if i % 97 == 3])
        self.assertLess(t.elapsed, 2.0)
