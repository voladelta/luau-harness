This is a small Python double-entry bookkeeping package (`ledger/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Add an account register (the running-balance statement you get from a bank):

1. `Journal.entries_for(account)` returns `[(entry, cents)]` in posting order for every entry that touches `account`, where `cents` is the net amount of that account inside the entry (an entry may contain several lines for the same account; sum them). An unknown account raises `ValueError`.
2. `reports.register(journal, account, start=None, end=None)` returns a list of dicts `{"date", "memo", "amount", "balance"}` in chronological order: sorted by entry date (`YYYY-MM-DD`), entries with equal dates keep their posting order. `amount` is the net amount from (1); `balance` is the running balance over ALL entries up to and including that row, even entries before `start`. Only rows with `start <= date <= end` are returned (either bound may be `None`).
3. `reports.format_register(rows)` returns one text line per row, joined with `"\n"` and no header, each formatted as `f"{date}  {memo:<20}  {fmt(amount):>12}  {fmt(balance):>12}"` using `money.fmt`.

Update or add tests as you see fit.
