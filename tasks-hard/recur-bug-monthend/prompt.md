This is a small Python library for recurring calendar events (`recur/`, a subset of RFC 5545 recurrence rules; tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Bug report:

> I created a monthly event starting on 2025-01-31 (`FREQ=MONTHLY`). The calendar shows it on Feb 28, and after that on Mar 28, Apr 28, May 28 and so on: it never goes back to the 31st. `FREQ=MONTHLY;BYMONTHDAY=30` shows up on Feb 28 too, and a yearly event created on 2024-02-29 lands on Feb 28 in 2025, 2026 and 2027.

Expected behaviour (this is what RFC 5545 specifies): a date that does not exist in a given month or year is skipped, never moved. The monthly event on the 31st happens only in months that have a 31st, a yearly event on Feb 29 happens only in leap years, and the series always stays on the day it was defined with. `COUNT` counts only dates that actually occur.

`Event.postpone` must keep its current behaviour: moving a single start date clamps it to the end of a shorter month.

Find and fix the bug. Don't change any public function names or signatures.
