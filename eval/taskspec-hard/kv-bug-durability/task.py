DOMAIN = "kv"
MUTATIONS = [
    ("kv/store.py", '''        # Tombstones may only go when nothing older than the merged segments is left for them to shadow.
        drop_tombstones = ids[0] == self.segment_ids[0]
        merged = {}
        for sid in ids:  # oldest first, newer overwrite
            merged.update(self.segments[sid])
        items = [(k, v) for k, v in merged.items() if not (drop_tombstones and v is TOMBSTONE)]''', '''        merged = {}
        for sid in ids:  # oldest first, newer overwrite
            merged.update(self.segments[sid])
        items = [(k, v) for k, v in merged.items() if v is not TOMBSTONE]  # deleted keys need no space any more'''),
    ("kv/store.py", '''        for sid in self.manifest["segments"]:  # oldest first, so newer layers overwrite
            view.update(self.segments[sid])''', '''        for sid in reversed(self.manifest["segments"]):  # newest segment first, like _lookup
            view.update(self.segments[sid])'''),
    ("kv/wal.py", '''        """Records of the log. A torn or corrupt tail (a crash mid-write) is cut off, so that new records are
        appended right after the last valid one."""
        if not os.path.exists(self.path):
            return []
        with open(self.path, "rb") as f:
            data = f.read()
        records, valid = decode_all(data)
        if valid < len(data):
            with open(self.path, "r+b") as f:
                f.truncate(valid)
        return records''', '''        """Records of the log, up to a torn or corrupt tail (a crash mid-write), which is ignored."""
        if not os.path.exists(self.path):
            return []
        with open(self.path, "rb") as f:
            data = f.read()
        records, _ = decode_all(data)
        return records'''),
]
