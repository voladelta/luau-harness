This is a small Python log-structured key-value store (`kv/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`). Changes go to a write-ahead log and an in-memory table, which is flushed into immutable segment files; `compact()` merges segments.

Three problems reported by the team running it in production:

> 1. Keys we deleted come back after compaction. It only happens to keys that were written long ago and deleted later. Our maintenance job only merges the newest few segments (`compact(n)`); a full `compact()` seems fine.
> 2. `scan()` and `keys()` sometimes return an old value of a key, or a key we deleted, while `get()` on the same key gives the right answer. Restarting doesn't help.
> 3. After a power cut the store opened fine, with only the last write before the cut missing (expected). But every write since then was gone after the next restart, even though those writes had succeeded.

Find and fix the bugs. Keep the on-disk format compatible with existing stores, and don't change any public function names or signatures.
