DOMAIN = "ledger"
MUTATIONS = [("ledger/money.py", 'int(frac.ljust(2, "0") or 0)', "int(frac or 0)")]
