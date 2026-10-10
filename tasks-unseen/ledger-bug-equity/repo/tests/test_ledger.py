import unittest

from ledger.accounts import Chart
from ledger.importer import import_csv
from ledger.journal import Entry, Journal
from ledger.money import fmt, parse_amount
from ledger.reports import balance_sheet, income_statement


def make():
    c = Chart()
    for n, t in [("cash", "asset"), ("loan", "liability"), ("capital", "equity"), ("sales", "income"), ("rent", "expense")]:
        c.add(n, t)
    return Journal(c)


class LedgerTests(unittest.TestCase):
    def test_money(self):
        self.assertEqual(parse_amount("1,200.5"), 120050)
        self.assertEqual(fmt(-1234), "-12.34")

    def test_post_and_reports(self):
        j = make()
        j.post(Entry("2026-01-01", "seed", [("cash", 10000), ("capital", -10000)]))
        j.post(Entry("2026-01-02", "sale", [("cash", 5000), ("sales", -5000)]))
        j.post(Entry("2026-01-03", "rent", [("rent", 2000), ("cash", -2000)]))
        self.assertEqual(balance_sheet(j), {"assets": 13000, "liabilities": 0, "equity": 13000})
        self.assertEqual(income_statement(j, "2026-01-01", "2026-01-31")["net"], 3000)
        with self.assertRaises(ValueError):
            j.post(Entry("2026-01-04", "bad", [("cash", 1)]))

    def test_import(self):
        j = make()
        n = import_csv("date,memo,debit_account,credit_account,amount\n2026-01-02,sale,cash,sales,12.50\n", j)
        self.assertEqual((n, j.balance("cash")), (1, 1250))


if __name__ == "__main__":
    unittest.main()
