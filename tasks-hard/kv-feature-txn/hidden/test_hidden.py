import os
import shutil
import tempfile
import unittest

from kv.store import Store
from kv.txn import ConflictError, TxnError


class T(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = os.path.join(self.tmp.name, "db")

    def tearDown(self):
        self.tmp.cleanup()

    def store(self, d=None):
        return Store(d or self.dir, memtable_limit=10_000)

    def wal_size(self):
        return os.path.getsize(os.path.join(self.dir, "wal.log"))

    def test_atomic_commit_every_cut(self):
        s = self.store()
        s.put("a", 1)
        s.put("gone", 0)
        before = self.wal_size()
        t = s.transaction()
        t.put("a", 2)
        t.put("b", "x" * 20)
        t.delete("gone")
        t.commit()
        after = self.wal_size()
        self.assertGreater(after, before)
        with open(os.path.join(self.dir, "wal.log"), "rb") as f:
            data = f.read()
        for cut in range(before, after + 1):
            crash = os.path.join(self.tmp.name, f"crash{cut}")
            shutil.copytree(self.dir, crash)
            with open(os.path.join(crash, "wal.log"), "wb") as f:
                f.write(data[:cut])
            got = self.store(crash).scan()
            if cut == after:
                self.assertEqual(got, [("a", 2), ("b", "x" * 20)])
            else:
                self.assertEqual(got, [("a", 1), ("gone", 0)], f"cut at {cut}")
            shutil.rmtree(crash)

    def test_atomic_after_reopen_and_flush(self):
        s = self.store()
        with s.transaction() as t:
            t.put("k1", 1)
            t.put("k2", 2)
        s = self.store()
        self.assertEqual(s.scan(), [("k1", 1), ("k2", 2)])
        s.flush()
        with s.transaction() as t:
            t.delete("k1")
            t.put("k3", 3)
        self.assertEqual(self.store().scan(), [("k2", 2), ("k3", 3)])

    def test_empty_transaction_writes_nothing(self):
        s = self.store()
        s.put("a", 1)
        size = self.wal_size()
        with s.transaction() as t:
            self.assertEqual(t.get("a"), 1)
        self.assertEqual(self.wal_size(), size)

    def test_old_wal_still_opens(self):
        os.makedirs(self.dir)
        from kv.codec import encode
        with open(os.path.join(self.dir, "wal.log"), "wb") as f:
            f.write(encode({"op": "put", "k": "a", "v": 1}) + encode({"op": "put", "k": "b", "v": 2}) + encode({"op": "del", "k": "a"}))
        self.assertEqual(self.store().scan(), [("b", 2)])

    def test_nested_visibility(self):
        s = self.store()
        s.put("a", 1)
        s.put("b", 1)
        t = s.transaction()
        t.put("a", 2)
        c = t.begin()
        self.assertEqual(c.get("a"), 2)
        c.put("b", 3)
        c.delete("a")
        self.assertIsNone(c.get("a"))
        g = c.begin()
        self.assertEqual((g.get("a", "dflt"), g.get("b")), ("dflt", 3))
        g.put("c", 9)
        g.commit()
        self.assertEqual(c.get("c"), 9)
        c.commit()
        self.assertEqual((t.get("a"), t.get("b"), t.get("c")), (None, 3, 9))
        self.assertEqual((s.get("a"), s.get("b"), s.get("c")), (1, 1, None))
        t.commit()
        self.assertEqual(self.store().scan(), [("b", 3), ("c", 9)])

    def test_child_rollback(self):
        s = self.store()
        t = s.transaction()
        t.put("a", 1)
        c = t.begin()
        c.put("a", 2)
        c.put("b", 2)
        c.rollback()
        self.assertEqual((t.get("a"), t.get("b")), (1, None))
        with t.begin() as c2:
            c2.put("z", 26)
        with self.assertRaises(KeyError):
            with t.begin() as c3:
                c3.put("y", 25)
                raise KeyError("boom")
        t.commit()
        self.assertEqual(self.store().scan(), [("a", 1), ("z", 26)])

    def test_parent_blocked_while_child_open(self):
        s = self.store()
        t = s.transaction()
        c = t.begin()
        for call in (lambda: t.get("a"), lambda: t.put("a", 1), lambda: t.delete("a"), t.begin, t.commit):
            with self.assertRaises(TxnError):
                call()
        c.put("a", 1)
        c.commit()
        t.put("b", 2)
        t.commit()
        self.assertEqual(s.scan(), [("a", 1), ("b", 2)])

    def test_parent_rollback_rolls_back_child(self):
        s = self.store()
        t = s.transaction()
        t.put("a", 1)
        c = t.begin()
        c.put("b", 2)
        t.rollback()
        for call in (lambda: c.put("x", 1), c.commit, lambda: t.get("a"), t.commit):
            with self.assertRaises(TxnError):
                call()
        self.assertEqual(s.scan(), [])

    def test_finished_transactions(self):
        s = self.store()
        t = s.transaction()
        t.put("a", 1)
        t.commit()
        for call in (lambda: t.get("a"), lambda: t.put("a", 1), t.commit, t.rollback, t.begin):
            with self.assertRaises(TxnError):
                call()
        c_parent = s.transaction()
        c = c_parent.begin()
        c.commit()
        with self.assertRaises(TxnError):
            c.put("x", 1)
        c_parent.commit()

    def test_conflict_with_direct_write(self):
        s = self.store()
        s.put("a", 1)
        t = s.transaction()
        t.put("a", 2)
        t.put("b", 2)
        s.put("a", 1)  # same value still counts
        with self.assertRaises(ConflictError):
            t.commit()
        self.assertEqual(s.scan(), [("a", 1)])
        with self.assertRaises(TxnError):
            t.put("c", 1)

    def test_conflict_between_transactions(self):
        s = self.store()
        t1, t2 = s.transaction(), s.transaction()
        t1.put("x", 1)
        c = t2.begin()
        c.put("x", 2)
        c.commit()
        t2.put("y", 2)
        t1.commit()
        with self.assertRaises(ConflictError):
            t2.commit()
        self.assertEqual(self.store().scan(), [("x", 1)])

    def test_conflict_on_delete_and_context_manager(self):
        s = self.store()
        s.put("a", 1)
        with self.assertRaises(ConflictError):
            with s.transaction() as t:
                t.put("a", 5)
                s.delete("a")
        self.assertIsNone(s.get("a"))

    def test_no_conflict(self):
        s = self.store()
        s.put("a", 1)
        t = s.transaction()
        self.assertEqual(t.get("a"), 1)
        t.put("b", 2)
        s.put("a", 7)  # only read by t
        s.put("c", 3)
        t.commit()
        self.assertEqual(s.scan(), [("a", 7), ("b", 2), ("c", 3)])
        s.put("b", 4)  # changes before the transaction began don't count
        t2 = s.transaction()
        t2.put("b", 5)
        t2.commit()
        self.assertEqual(s.get("b"), 5)
