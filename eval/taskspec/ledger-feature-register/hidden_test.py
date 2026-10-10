import unittest

from ledger.accounts import Chart
from ledger.journal import Entry, Journal
from ledger.reports import format_register, register


def make():
    c = Chart()
    for n, t in [("cash", "asset"), ("sales", "income"), ("rent", "expense"), ("capital", "equity")]:
        c.add(n, t)
    j = Journal(c)
    j.post(Entry("2026-01-05", "rent", [("rent", 2000), ("cash", -2000)]))
    j.post(Entry("2026-01-01", "seed", [("cash", 10000), ("capital", -10000)]))
    j.post(Entry("2026-01-02", "x", [("cash", 300), ("sales", -300)]))
    j.post(Entry("2026-01-02", "y", [("cash", 700), ("sales", -700)]))
    j.post(Entry("2026-01-03", "split", [("cash", 500), ("cash", 500), ("sales", -1000)]))
    return j


class T(unittest.TestCase):
    def test_entries_for(self):
        j = make()
        got = j.entries_for("cash")
        self.assertEqual([(e.memo, c) for e, c in got], [("rent", -2000), ("seed", 10000), ("x", 300), ("y", 700), ("split", 1000)])
        self.assertEqual([e.memo for e, _ in j.entries_for("capital")], ["seed"])
        with self.assertRaises(ValueError):
            j.entries_for("nope")

    def test_register(self):
        rows = register(make(), "cash")
        self.assertEqual([(r["date"], r["memo"], r["amount"], r["balance"]) for r in rows], [
            ("2026-01-01", "seed", 10000, 10000), ("2026-01-02", "x", 300, 10300), ("2026-01-02", "y", 700, 11000),
            ("2026-01-03", "split", 1000, 12000), ("2026-01-05", "rent", -2000, 10000)])

    def test_range_keeps_opening_balance(self):
        rows = register(make(), "cash", start="2026-01-02", end="2026-01-03")
        self.assertEqual([(r["memo"], r["balance"]) for r in rows], [("x", 10300), ("y", 11000), ("split", 12000)])
        self.assertEqual(register(make(), "cash", start="2026-02-01"), [])
        self.assertEqual(len(register(make(), "cash", end="2026-01-01")), 1)

    def test_format(self):
        rows = register(make(), "cash", end="2026-01-01")
        want = "2026-01-01  " + "seed" + " " * 16 + "  " + " " * 6 + "100.00" + "  " + " " * 6 + "100.00"
        self.assertEqual(format_register(rows), want)
        self.assertEqual(format_register([]), "")
