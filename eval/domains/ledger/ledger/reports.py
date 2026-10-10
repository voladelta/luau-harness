def _sum(journal, type_):
    return sum(b for a, b in journal.trial_balance().items() if journal.chart.type_of(a) == type_)


def balance_sheet(journal):
    """Assets = liabilities + equity (+ retained earnings). Credits are negative internally."""
    assets = _sum(journal, "asset")
    liabilities = -_sum(journal, "liability")
    equity = -_sum(journal, "equity")
    retained = -(_sum(journal, "income") + _sum(journal, "expense"))
    return {"assets": assets, "liabilities": liabilities, "equity": equity + retained}


def income_statement(journal, start, end):
    """Dates are YYYY-MM-DD strings; start and end both inclusive."""
    income = expense = 0
    for e in journal.entries:
        if start <= e.date <= end:
            for a, c in e.lines:
                t = journal.chart.type_of(a)
                if t == "income":
                    income += -c
                elif t == "expense":
                    expense += c
    return {"income": income, "expenses": expense, "net": income - expense}
