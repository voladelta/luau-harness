from dataclasses import dataclass


@dataclass
class Entry:
    date: str  # YYYY-MM-DD
    memo: str
    lines: list  # [(account, cents)]: debit positive, credit negative; must sum to zero


class Journal:
    def __init__(self, chart):
        self.chart = chart
        self.entries = []

    def post(self, entry):
        if sum(c for _, c in entry.lines) != 0:
            raise ValueError(f"unbalanced entry: {entry.memo}")
        for acct, _ in entry.lines:
            self.chart.type_of(acct)
        self.entries.append(entry)

    def balance(self, account):
        return sum(c for e in self.entries for a, c in e.lines if a == account)

    def trial_balance(self):
        return {a: self.balance(a) for a in self.chart.accounts}
