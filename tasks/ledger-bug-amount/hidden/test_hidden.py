import unittest

from ledger.accounts import Chart
from ledger.importer import import_csv
from ledger.journal import Journal
from ledger.money import parse_amount


class T(unittest.TestCase):
    def test_parse(self):
        for s, want in [("10.5", 1050), ("0.5", 50), ("7", 700), ("-3.7", -370), ("1,200.5", 120050), ("2.25", 225), ("0.05", 5), ("5.", 500)]:
            self.assertEqual(parse_amount(s), want, s)
        for bad in ["1.234", "abc", "1.2.3", ""]:
            with self.assertRaises(ValueError):
                parse_amount(bad)

    def test_import(self):
        c = Chart()
        c.add("cash", "asset")
        c.add("sales", "income")
        j = Journal(c)
        n = import_csv("date,memo,debit_account,credit_account,amount\n2026-01-02,a,cash,sales,10.5\n2026-01-03,b,cash,sales,0.5\n", j)
        self.assertEqual((n, j.balance("cash"), j.balance("sales")), (2, 1100, -1100))
