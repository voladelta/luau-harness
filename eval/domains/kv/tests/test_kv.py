import os
import tempfile
import unittest

from kv.store import Store
from kv.txn import TxnError


class KvTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name

    def tearDown(self):
        self.tmp.cleanup()

    def test_put_get_delete(self):
        s = Store(self.dir)
        s.put("a", 1)
        s.put("b", {"x": [1, 2]})
        s.delete("a")
        self.assertIsNone(s.get("a"))
        self.assertEqual(s.get("b"), {"x": [1, 2]})
        self.assertNotIn("a", s)

    def test_reopen_replays_wal(self):
        s = Store(self.dir)
        s.put("a", 1)
        s.delete("a")
        s.put("b", 2)
        self.assertEqual(Store(self.dir).scan(), [("b", 2)])

    def test_flush_and_layers(self):
        s = Store(self.dir, memtable_limit=1000)
        s.put("a", 1)
        s.flush()
        s.put("a", 2)
        self.assertEqual(s.get("a"), 2)
        s.flush()
        self.assertEqual(Store(self.dir).get("a"), 2)
        self.assertEqual(s.stats()["segments"], 2)

    def test_auto_flush(self):
        s = Store(self.dir, memtable_limit=3)
        for i in range(7):
            s.put(f"k{i}", i)
        self.assertEqual(s.stats()["segments"], 2)
        self.assertEqual(len(Store(self.dir).keys("k")), 7)

    def test_compact_all(self):
        s = Store(self.dir, memtable_limit=1000)
        s.put("a", 1)
        s.flush()
        s.delete("a")
        s.put("b", 1)
        s.flush()
        s.compact()
        self.assertEqual(s.scan(), [("b", 1)])
        self.assertEqual(len([f for f in os.listdir(self.dir) if f.startswith("seg-")]), 1)

    def test_torn_tail_ignored(self):
        s = Store(self.dir)
        s.put("a", 1)
        with open(os.path.join(self.dir, "wal.log"), "ab") as f:
            f.write(b"0badc0de {\"op\":")
        self.assertEqual(Store(self.dir).get("a"), 1)

    def test_transaction(self):
        s = Store(self.dir)
        s.put("a", 1)
        with s.transaction() as t:
            t.put("a", 2)
            t.delete("b")
            self.assertEqual(t.get("a"), 2)
            self.assertEqual(s.get("a"), 1)
        self.assertEqual(s.get("a"), 2)
        with self.assertRaises(TxnError):
            t.put("c", 1)

    def test_transaction_rollback_on_error(self):
        s = Store(self.dir)
        with self.assertRaises(RuntimeError):
            with s.transaction() as t:
                t.put("a", 1)
                raise RuntimeError("boom")
        self.assertIsNone(s.get("a"))


if __name__ == "__main__":
    unittest.main()
