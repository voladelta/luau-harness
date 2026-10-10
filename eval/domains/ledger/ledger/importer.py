import csv
import io

from .journal import Entry
from .money import parse_amount


def import_csv(text, journal):
    """Rows: date,memo,debit_account,credit_account,amount. Returns number of entries posted."""
    n = 0
    for row in csv.DictReader(io.StringIO(text)):
        cents = parse_amount(row["amount"])
        journal.post(Entry(row["date"], row["memo"], [(row["debit_account"], cents), (row["credit_account"], -cents)]))
        n += 1
    return n
