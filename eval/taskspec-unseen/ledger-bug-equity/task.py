DOMAIN = "ledger"
MUTATIONS = [("ledger/reports.py", '    retained = -(_sum(journal, "income") + _sum(journal, "expense"))', '    retained = _sum(journal, "income") + _sum(journal, "expense")')]
