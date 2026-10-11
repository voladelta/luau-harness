"""Transactions: writes are buffered and applied to the store on commit."""
from .segment import TOMBSTONE


class TxnError(Exception):
    pass


class Transaction:
    def __init__(self, store):
        self.store = store
        self.writes = {}
        self.done = False

    def _live(self):
        if self.done:
            raise TxnError("transaction is finished")

    def get(self, key, default=None):
        self._live()
        if key in self.writes:
            v = self.writes[key]
            return default if v is TOMBSTONE else v
        return self.store.get(key, default)

    def put(self, key, value):
        self._live()
        self.store._check(key, value)
        self.writes[key] = value

    def delete(self, key):
        self._live()
        self.store._check(key)
        self.writes[key] = TOMBSTONE

    def commit(self):
        self._live()
        for k, v in self.writes.items():
            if v is TOMBSTONE:
                self.store.delete(k)
            else:
                self.store.put(k, v)
        self.done = True

    def rollback(self):
        self._live()
        self.writes.clear()
        self.done = True

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
