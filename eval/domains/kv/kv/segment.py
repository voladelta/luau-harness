"""Immutable sorted segment files. A segment maps key -> value, or key -> TOMBSTONE for a deletion that must keep
shadowing older segments."""
import os

from .codec import CorruptError, decode_all, encode


class _Tombstone:
    def __repr__(self):
        return "TOMBSTONE"


TOMBSTONE = _Tombstone()


def write_segment(path, items):
    """items: iterable of (key, value or TOMBSTONE). Written to a temp file, then renamed into place."""
    tmp = path + ".tmp"
    with open(tmp, "wb") as f:
        for k, v in sorted(items, key=lambda kv: kv[0]):
            f.write(encode({"k": k, "del": True} if v is TOMBSTONE else {"k": k, "v": v}))
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


def read_segment(path):
    with open(path, "rb") as f:
        data = f.read()
    records, valid = decode_all(data)
    if valid != len(data):
        raise CorruptError(f"corrupt segment {path}")
    return {r["k"]: TOMBSTONE if r.get("del") else r["v"] for r in records}
