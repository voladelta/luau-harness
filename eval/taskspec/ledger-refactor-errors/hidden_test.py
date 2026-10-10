import unittest

from ledger import errors
from ledger.accounts import Chart
from ledger.errors import (DuplicateAccount, ImportRowError, InvalidAccountType, InvalidAmount, LedgerError,
                           UnbalancedEntry, UnknownAccount)
from ledger.importer import import_csv
from ledger.journal import Entry, Journal
from ledger.money import parse_amount

HEADER = "date,memo,debit_account,credit_account,amount\n"


def chart():
    c = Chart()
    c.add("cash", "asset")
    c.add("sales", "income")
    return c


class T(unittest.TestCase):
    def test_hierarchy(self):
        for cls in (InvalidAmount, InvalidAccountType, DuplicateAccount, UnknownAccount, UnbalancedEntry, ImportRowError):
            self.assertTrue(issubclass(cls, LedgerError), cls)
            self.assertTrue(issubclass(cls, ValueError), cls)
        self.assertTrue(issubclass(LedgerError, Exception) and not issubclass(LedgerError, ValueError))

    def test_money_and_accounts(self):
        with self.assertRaises(InvalidAmount) as cm:
            parse_amount("abc")
        self.assertEqual(str(cm.exception), "bad amount: 'abc'")
        c = chart()
        with self.assertRaisesRegex(InvalidAccountType, "^bad account type: wat$"):
            c.add("x", "wat")
        with self.assertRaisesRegex(DuplicateAccount, "^duplicate account: cash$"):
            c.add("cash", "asset")
        with self.assertRaisesRegex(UnknownAccount, "^unknown account: zzz$"):
            c.type_of("zzz")

    def test_journal(self):
        j = Journal(chart())
        with self.assertRaisesRegex(UnbalancedEntry, "^unbalanced entry: bad$"):
            j.post(Entry("2026-01-01", "bad", [("cash", 1)]))
        with self.assertRaises(UnknownAccount):
            j.post(Entry("2026-01-01", "q", [("cash", 1), ("nowhere", -1)]))

    def test_import_row_error(self):
        j = Journal(chart())
        text = HEADER + "2026-01-01,ok,cash,sales,5\n2026-01-02,bad,cash,sales,x\n2026-01-03,never,cash,sales,1\n"
        with self.assertRaises(ImportRowError) as cm:
            import_csv(text, j)
        e = cm.exception
        self.assertEqual(e.line, 3)
        self.assertIsInstance(e.cause, InvalidAmount)
        self.assertIs(e.__cause__, e.cause)
        self.assertEqual(str(e), "line 3: bad amount: 'x'")
        self.assertEqual(len(j.entries), 1)

    def test_import_unknown_account_line(self):
        with self.assertRaises(ImportRowError) as cm:
            import_csv(HEADER + "2026-01-01,a,cash,nope,5\n", Journal(chart()))
        self.assertEqual((cm.exception.line, type(cm.exception.cause)), (2, UnknownAccount))
        self.assertEqual(import_csv(HEADER + "2026-01-01,a,cash,sales,5\n", Journal(chart())), 1)

    def test_no_stray_valueerror_raises(self):
        self.assertTrue(hasattr(errors, "LedgerError"))
