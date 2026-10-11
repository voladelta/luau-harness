"""Transactions: writes are buffered and applied to the store on commit, atomically.

A transaction can open one child at a time (begin()); a child's commit folds its writes into the parent, and only
the top-level commit reaches the store, as a single log record.
"""
from .segment import TOMBSTONE


class TxnError(Exception):
    pass


class ConflictError(TxnError):
    pass


class Transaction:
    def __init__(self, store, parent=None):
        self.store = store
        self.parent = parent
        self.child = None
        self.writes = {}
        self.done = False
        self.start_version = store.version if parent is None else None

    def _live(self):
        if self.done:
            raise TxnError("transaction is finished")
        if self.child is not None:
            raise TxnError("a child transaction is still open")

    def _lookup(self, key):
        t = self
        while t is not None:
            if key in t.writes:
                return t.writes[key]
            t = t.parent
        return self.store._lookup(key)

    def get(self, key, default=None):
        self._live()
        v = self._lookup(key)
        return default if v is TOMBSTONE else v

    def put(self, key, value):
        self._live()
        self.store._check(key, value)
        self.writes[key] = value

    def delete(self, key):
        self._live()
        self.store._check(key)
        self.writes[key] = TOMBSTONE

    def begin(self):
        self._live()
        self.child = Transaction(self.store, self)
        return self.child

    def commit(self):
        self._live()
        if self.parent is not None:
            self.parent.writes.update(self.writes)
        else:
            conflicts = sorted(k for k in self.writes if self.store.changed.get(k, 0) > self.start_version)
            if conflicts:
                self._finish()
                raise ConflictError(f"changed since the transaction began: {', '.join(conflicts)}")
            self.store._commit_batch(self.writes)
        self._finish()

    def rollback(self):
        if self.done:
            raise TxnError("transaction is finished")
        if self.child is not None:
            self.child.rollback()
        self._finish()

    def _finish(self):
        self.writes = {}
        self.done = True
        if self.parent is not None and self.parent.child is self:
            self.parent.child = None

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        if self.done:
            return False
        if exc_type is None:
            self.commit()
        else:
            self.rollback()
        return False
