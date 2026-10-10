This is a small Python shopping-cart package (`shop/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Bug report: receipts for big orders show amounts like `$1234.50` and `$12000.00`. They should be formatted with thousands separators, like `$1,234.50` and `$12,000.00`. Small amounts look fine.

Find and fix the bug. Don't change any public function names or signatures.
