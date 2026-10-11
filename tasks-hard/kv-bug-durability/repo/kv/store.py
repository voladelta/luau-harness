"""A small log-structured key-value store.

Layers, newest first: the memtable (recent changes, also recorded in the WAL), then the segments listed in the
manifest from newest to oldest. A lookup stops at the first layer that has the key; a TOMBSTONE there means the key
is deleted, whatever older layers say.

Files in the store directory: wal.log, MANIFEST.json, seg-NNNNNN.dat.
"""
import json
import os

from . import manifest as mf
from .segment import TOMBSTONE, read_segment, write_segment
from .txn import Transaction
from .wal import WAL

WAL_NAME = "wal.log"


class Store:
    def __init__(self, directory, memtable_limit=64):
        self.dir = directory
        self.memtable_limit = memtable_limit
        os.makedirs(directory, exist_ok=True)
        self.manifest = mf.load(directory)
        self.segments = {sid: read_segment(self._seg_path(sid)) for sid in self.manifest["segments"]}
        self.memtable = {}
        self.wal = WAL(os.path.join(directory, WAL_NAME))
        for record in self.wal.replay():
            self._apply(record)

    # -- internals
    def _seg_path(self, sid):
        return os.path.join(self.dir, mf.segment_name(sid))

    @property
    def segment_ids(self):
        """Live segment ids, oldest first."""
        return list(self.manifest["segments"])

    def _apply(self, record):
        if record["op"] == "put":
            self.memtable[record["k"]] = record["v"]
        elif record["op"] == "del":
            self.memtable[record["k"]] = TOMBSTONE
        else:
            raise ValueError(f"unknown log record {record!r}")

    def _write(self, record):
        self.wal.append(record)
        self._apply(record)
        if len(self.memtable) >= self.memtable_limit:
            self.flush()

    @staticmethod
    def _check(key, value=None):
        if not isinstance(key, str) or not key:
            raise TypeError("keys must be non-empty strings")
        try:
            json.dumps(value)
        except TypeError:
            raise TypeError(f"value for {key!r} is not JSON-serializable") from None

    # -- reads
    def _lookup(self, key):
        if key in self.memtable:
            return self.memtable[key]
        for sid in reversed(self.manifest["segments"]):
            seg = self.segments[sid]
            if key in seg:
                return seg[key]
        return TOMBSTONE

    def get(self, key, default=None):
        v = self._lookup(key)
        return default if v is TOMBSTONE else v

    def __contains__(self, key):
        return self._lookup(key) is not TOMBSTONE

    def scan(self, prefix=""):
        """Sorted [(key, value)] of live keys starting with prefix."""
        view = {}
        for sid in reversed(self.manifest["segments"]):  # newest segment first, like _lookup
            view.update(self.segments[sid])
        view.update(self.memtable)
        return sorted((k, v) for k, v in view.items() if k.startswith(prefix) and v is not TOMBSTONE)

    def keys(self, prefix=""):
        return [k for k, _ in self.scan(prefix)]

    # -- writes
    def put(self, key, value):
        self._check(key, value)
        self._write({"op": "put", "k": key, "v": value})

    def delete(self, key):
        self._check(key)
        self._write({"op": "del", "k": key})

    def transaction(self):
        return Transaction(self)

    # -- maintenance
    def flush(self):
        """Write the memtable out as the newest segment and start a fresh log."""
        if not self.memtable:
            return None
        sid = self.manifest["next_id"]
        write_segment(self._seg_path(sid), self.memtable.items())
        self.segments[sid] = dict(self.memtable)
        self.manifest = {"segments": self.manifest["segments"] + [sid], "next_id": sid + 1}
        mf.save(self.dir, self.manifest)
        self.wal.reset()
        self.memtable = {}
        return sid

    def compact(self, n=None):
        """Merge the newest n segments (default: all) into one. Returns the new segment id, or None."""
        ids = self.segment_ids
        if n is not None:
            ids = ids[-n:] if n > 0 else []
        if len(ids) < 2:
            return None
        merged = {}
        for sid in ids:  # oldest first, newer overwrite
            merged.update(self.segments[sid])
        items = [(k, v) for k, v in merged.items() if v is not TOMBSTONE]  # deleted keys need no space any more
        new = self.manifest["next_id"]
        write_segment(self._seg_path(new), items)
        self.segments[new] = dict(items)
        keep = [sid for sid in self.segment_ids if sid not in ids]
        self.manifest = {"segments": keep + [new], "next_id": new + 1}
        mf.save(self.dir, self.manifest)
        for sid in ids:
            del self.segments[sid]
            os.remove(self._seg_path(sid))
        return new

    def stats(self):
        return {"segments": len(self.manifest["segments"]), "memtable": len(self.memtable), "wal_bytes": self.wal.size()}
