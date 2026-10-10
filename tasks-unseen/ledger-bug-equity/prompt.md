This is a small Python double-entry bookkeeping package (`ledger/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Bug report: after booking sales and expenses, the balance sheet no longer balances. Assets are higher than liabilities plus equity by twice the profit (or lower by twice the loss). With only the initial capital booked it balances correctly.

Find and fix the bug. Don't change any public function names or signatures.
