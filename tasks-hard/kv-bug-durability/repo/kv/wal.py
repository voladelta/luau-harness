"""Write-ahead log: every change is appended (and fsynced) here before it touches the memtable."""
import os

from .codec import decode_all, encode


class WAL:
    def __init__(self, path):
        self.path = path

    def replay(self):
        """Records of the log, up to a torn or corrupt tail (a crash mid-write), which is ignored."""
        if not os.path.exists(self.path):
            return []
        with open(self.path, "rb") as f:
            data = f.read()
        records, _ = decode_all(data)
        return records

    def append(self, record):
        with open(self.path, "ab") as f:
            f.write(encode(record))
            f.flush()
            os.fsync(f.fileno())

    def reset(self):
        with open(self.path, "wb") as f:
            f.flush()
            os.fsync(f.fileno())

    def size(self):
        return os.path.getsize(self.path) if os.path.exists(self.path) else 0
