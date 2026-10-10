DOMAIN = "ledger"

def solve(h):
    h.edit("ledger/journal.py", "    def balance(self, account):", '''    def entries_for(self, account):
        self.chart.type_of(account)
        return [(e, sum(c for a, c in e.lines if a == account)) for e in self.entries if any(a == account for a, _ in e.lines)]

    def balance(self, account):''')
    h.edit("ledger/reports.py", "def _sum(journal, type_):", "from .money import fmt\n\n\ndef _sum(journal, type_):")
    h.write("ledger/reports.py", (h.ws / "ledger/reports.py").read_text() + '''

def register(journal, account, start=None, end=None):
    rows = sorted(journal.entries_for(account), key=lambda p: p[0].date)
    bal, out = 0, []
    for e, amt in rows:
        bal += amt
        if (start is None or e.date >= start) and (end is None or e.date <= end):
            out.append({"date": e.date, "memo": e.memo, "amount": amt, "balance": bal})
    return out


def format_register(rows):
    return "\\n".join(f"{r['date']}  {r['memo']:<20}  {fmt(r['amount']):>12}  {fmt(r['balance']):>12}" for r in rows)
''')
