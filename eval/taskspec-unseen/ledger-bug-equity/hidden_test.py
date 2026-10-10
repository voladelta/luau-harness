import unittest

from ledger.accounts import Chart
from ledger.journal import Entry, Journal
from ledger.reports import balance_sheet


def make():
    c = Chart()
    for n, t in [("cash", "asset"), ("loan", "liability"), ("capital", "equity"), ("sales", "income"), ("rent", "expense")]:
        c.add(n, t)
    return Journal(c)


class T(unittest.TestCase):
    def test_balances_with_profit(self):
        j = make()
        j.post(Entry("2026-01-01", "seed", [("cash", 10000), ("capital", -10000)]))
        j.post(Entry("2026-01-02", "sale", [("cash", 5000), ("sales", -5000)]))
        j.post(Entry("2026-01-03", "rent", [("rent", 2000), ("cash", -2000)]))
        self.assertEqual(balance_sheet(j), {"assets": 13000, "liabilities": 0, "equity": 13000})

    def test_balances_with_loss_and_loan(self):
        j = make()
        j.post(Entry("2026-01-01", "loan", [("cash", 4000), ("loan", -4000)]))
        j.post(Entry("2026-01-02", "rent", [("rent", 1500), ("cash", -1500)]))
        bs = balance_sheet(j)
        self.assertEqual(bs, {"assets": 2500, "liabilities": 4000, "equity": -1500})
        self.assertEqual(bs["assets"], bs["liabilities"] + bs["equity"])
