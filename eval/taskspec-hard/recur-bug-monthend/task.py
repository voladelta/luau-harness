DOMAIN = "recur"
MUTATIONS = [
    ("recur/dates.py", '''def make_date(year, month, day):
    """date(year, month, day), or None when the month has no such day (e.g. February 30)."""
    if 1 <= day <= days_in_month(year, month):
        return date(year, month, day)
    return None''', '''def make_date(year, month, day):
    """date(year, month, day); a day past the end of the month means the month's last day (e.g. February 30)."""
    return date(year, month, min(day, days_in_month(year, month)))'''),
    ("recur/expand.py", "from .dates import add_months, make_date, month_days, week_start",
     "from .dates import make_date, month_days, shift_months, week_start"),
    ("recur/expand.py", '''        i = 0
        while True:
            y, m = add_months(start.year, start.month, i * rule.interval)
            yield date(y, m, 1), month_candidates(rule, y, m, start)
            i += 1''', '''        anchor = start
        while True:
            yield date(anchor.year, anchor.month, 1), month_candidates(rule, anchor.year, anchor.month, anchor)
            anchor = shift_months(anchor, rule.interval)'''),
]
