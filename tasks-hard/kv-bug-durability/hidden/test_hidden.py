import os
import tempfile
import unittest

from kv.store import Store


class T(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name

    def tearDown(self):
        self.tmp.cleanup()

    def store(self):
        return Store(self.dir, memtable_limit=10_000)

    def old_then_deleted(self):
        s = self.store()
        s.put("old", "v1")
        s.put("keep", 1)
        s.flush()  # segment 1 (oldest): old=v1
        s.put("x", 1)
        s.flush()
        s.delete("old")
        s.put("y", 2)
        s.flush()  # segment 3: tombstone for old
        s.put("z", 3)
        s.flush()
        return s

    def test_partial_compaction_keeps_tombstones(self):
        s = self.old_then_deleted()
        s.compact(2)
        self.assertIsNone(s.get("old"))
        self.assertNotIn("old", s.keys())
        self.assertIsNone(self.store().get("old"))
        s.compact(3)
        self.assertIsNone(s.get("old"))
        self.assertEqual(self.store().scan(), [("keep", 1), ("x", 1), ("y", 2), ("z", 3)])

    def test_full_compaction_drops_tombstones(self):
        s = self.old_then_deleted()
        s.compact(2)
        s.compact()
        self.assertEqual(s.stats()["segments"], 1)
        seg = [f for f in os.listdir(self.dir) if f.startswith("seg-")]
        with open(os.path.join(self.dir, seg[0]), "rb") as f:
            self.assertNotIn(b'"old"', f.read())
        self.assertIsNone(self.store().get("old"))

    def test_scan_prefers_newest(self):
        s = self.store()
        s.put("a", 1)
        s.put("b", 1)
        s.put("c", 1)
        s.flush()
        s.put("a", 2)
        s.delete("b")
        s.flush()
        s.put("a", 3)
        s.flush()
        s.put("d", 4)
        self.assertEqual(s.scan(), [("a", 3), ("c", 1), ("d", 4)])
        self.assertEqual(self.store().keys(), ["a", "c", "d"])
        self.assertEqual([s.get(k) for k in "abcd"], [3, None, 1, 4])
        s.compact(2)
        self.assertEqual(s.scan(), [("a", 3), ("c", 1), ("d", 4)])

    def test_writes_after_torn_tail_survive(self):
        s = self.store()
        s.put("a", 1)
        s.put("b", 2)
        wal = os.path.join(self.dir, "wal.log")
        with open(wal, "rb") as f:
            data = f.read()
        with open(wal, "wb") as f:
            f.write(data[:-7])  # power cut while writing "b"
        s = self.store()
        self.assertEqual((s.get("a"), s.get("b")), (1, None))
        s.put("c", 3)
        s.delete("a")
        s = self.store()
        self.assertEqual(s.scan(), [("c", 3)])
        s.put("d", 4)
        self.assertEqual(self.store().scan(), [("c", 3), ("d", 4)])

    def test_garbage_tail(self):
        s = self.store()
        s.put("a", 1)
        with open(os.path.join(self.dir, "wal.log"), "ab") as f:
            f.write(b"0badc0de {\"op\":\"put\"")
        s = self.store()
        s.put("b", 2)
        self.assertEqual(self.store().scan(), [("a", 1), ("b", 2)])

    def test_unchanged(self):
        s = Store(self.dir, memtable_limit=3)
        for i in range(10):
            s.put(f"k{i}", i)
        s.delete("k4")
        s.compact()
        self.assertEqual(len(self.store().keys("k")), 9)
        with s.transaction() as t:
            t.put("k4", "back")
        self.assertEqual(self.store().get("k4"), "back")
