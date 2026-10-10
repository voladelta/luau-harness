DOMAIN = "ledger"
CHECK = '''if grep -rn "raise ValueError" ledger/ --include=*.py; then echo "bare ValueError remains"; fail=1; fi'''

def solve(h):
    h.write("ledger/errors.py", '''class LedgerError(Exception):
    pass


class InvalidAmount(LedgerError, ValueError):
    pass


class InvalidAccountType(LedgerError, ValueError):
    pass


class DuplicateAccount(LedgerError, ValueError):
    pass


class UnknownAccount(LedgerError, ValueError):
    pass


class UnbalancedEntry(LedgerError, ValueError):
    pass


class ImportRowError(LedgerError, ValueError):
    def __init__(self, line, cause):
        super().__init__(f"line {line}: {cause}")
        self.line = line
        self.cause = cause
''')
    h.edit("ledger/money.py", 'raise ValueError(f"bad amount', 'raise InvalidAmount(f"bad amount')
    h.edit("ledger/money.py", "def parse_amount", "from .errors import InvalidAmount\n\n\ndef parse_amount")
    h.edit("ledger/accounts.py", 'raise ValueError(f"bad account type', 'raise InvalidAccountType(f"bad account type')
    h.edit("ledger/accounts.py", 'raise ValueError(f"duplicate', 'raise DuplicateAccount(f"duplicate')
    h.edit("ledger/accounts.py", 'raise ValueError(f"unknown', 'raise UnknownAccount(f"unknown')
    h.edit("ledger/accounts.py", "TYPES =", "from .errors import DuplicateAccount, InvalidAccountType, UnknownAccount\n\nTYPES =")
    h.edit("ledger/journal.py", 'raise ValueError(f"unbalanced', 'raise UnbalancedEntry(f"unbalanced')
    h.edit("ledger/journal.py", "from dataclasses import dataclass\n", "from dataclasses import dataclass\n\nfrom .errors import UnbalancedEntry\n")
    h.write("ledger/importer.py", '''import csv
import io

from .errors import ImportRowError, LedgerError
from .journal import Entry
from .money import parse_amount


def import_csv(text, journal):
    """Rows: date,memo,debit_account,credit_account,amount. Returns number of entries posted."""
    n = 0
    for line, row in enumerate(csv.DictReader(io.StringIO(text)), start=2):
        try:
            cents = parse_amount(row["amount"])
            journal.post(Entry(row["date"], row["memo"], [(row["debit_account"], cents), (row["credit_account"], -cents)]))
        except LedgerError as e:
            raise ImportRowError(line, e) from e
        n += 1
    return n
''')
