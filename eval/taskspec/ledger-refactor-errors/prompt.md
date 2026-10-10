This is a small Python double-entry bookkeeping package (`ledger/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Refactor: every error is a bare `ValueError` with a message, so callers cannot tell the cases apart. Introduce a proper exception hierarchy:

- New module `ledger/errors.py` with `LedgerError(Exception)` and these subclasses, each also inheriting `ValueError` so existing `except ValueError` code keeps working: `InvalidAmount`, `InvalidAccountType`, `DuplicateAccount`, `UnknownAccount`, `UnbalancedEntry`.
- Replace every `raise ValueError` in `ledger/` with the matching class (bad amount, bad account type, duplicate account, unknown account, unbalanced entry). Messages stay exactly as they are.
- Also in `errors.py`: `ImportRowError(LedgerError, ValueError)` with attributes `line` and `cause`, and message `f"line {line}: {cause}"`. `importer.import_csv` raises it when a row fails with any `LedgerError`; `line` is the 1-based line number in the CSV text, counting the header as line 1, so the first data row is line 2. Rows posted before the failing one stay posted. `cause` is the original exception and is also chained with `raise ... from`.
- `raise ValueError` must not appear anywhere in `ledger/` afterwards.

Update the existing tests.
