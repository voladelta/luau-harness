This is a small Python double-entry bookkeeping package (`ledger/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Bug report: importing a bank CSV row whose amount is written with a single decimal digit, such as `10.5`, books $10.05 instead of $10.50. Amounts with two decimals or none are fine.

Find and fix the bug. Don't change any public function names or signatures.
