"""Expand a Rule into concrete dates.

Each frequency walks periods (days, weeks, months or years, INTERVAL apart) starting at the period that contains
DTSTART. The BY* parts generate the candidate dates inside each period, or only filter them when the frequency is
finer than the part (BYMONTH under MONTHLY, BYDAY under DAILY, ...). Candidates before DTSTART are dropped.
"""
from datetime import date, timedelta

from .dates import add_months, make_date, month_days, week_start

HORIZON_YEARS = 400  # stop scanning a rule whose dates can never (or no longer) occur


def month_candidates(rule, year, month, start):
    """Candidate dates of one month for MONTHLY and YEARLY rules, sorted."""
    if rule.bymonthday:
        days = [make_date(year, month, n) for n in sorted(set(rule.bymonthday))]
        days = [d for d in days if d is not None]
        if rule.byday:
            days = [d for d in days if d.weekday() in rule.byday]
        return days
    if rule.byday:
        return [d for d in month_days(year, month) if d.weekday() in rule.byday]
    d = make_date(year, month, start.day)
    return [d] if d is not None else []


def periods(rule, start):
    """Yield (first day of period, sorted candidate dates) forever."""
    if rule.freq == "DAILY":
        d = start
        while True:
            yield d, [d]
            d += timedelta(days=rule.interval)
    elif rule.freq == "WEEKLY":
        ws = week_start(start)
        days = sorted(set(rule.byday)) if rule.byday else [start.weekday()]
        while True:
            yield ws, [ws + timedelta(days=w) for w in days]
            ws += timedelta(weeks=rule.interval)
    elif rule.freq == "MONTHLY":
        i = 0
        while True:
            y, m = add_months(start.year, start.month, i * rule.interval)
            yield date(y, m, 1), month_candidates(rule, y, m, start)
            i += 1
    else:  # YEARLY
        y = start.year
        while True:
            out = []
            for m in sorted(set(rule.bymonth)) if rule.bymonth else [start.month]:
                out += month_candidates(rule, y, m, start)
            yield date(y, 1, 1), out
            y += rule.interval


def keep(rule, d):
    """BY* parts that only filter at this frequency."""
    if rule.bymonth and d.month not in rule.bymonth:
        return False
    if rule.freq == "DAILY" and rule.byday and d.weekday() not in rule.byday:
        return False
    if rule.freq in ("DAILY", "WEEKLY") and rule.bymonthday and d.day not in rule.bymonthday:
        return False
    return True


def occurrences(rule, start, exdates=()):
    """Yield the rule's dates on or after start, in order.

    COUNT counts dates before EXDATE removal, as RFC 5545 specifies: an excluded date still uses up one of the
    COUNT occurrences. UNTIL is inclusive.
    """
    excluded = set(exdates)
    emitted = 0
    horizon = start.year + HORIZON_YEARS
    for first, candidates in periods(rule, start):
        if first.year > horizon or (rule.until is not None and first > rule.until):
            return
        for d in candidates:
            if d < start or not keep(rule, d):
                continue
            if rule.until is not None and d > rule.until:
                return
            emitted += 1
            if d not in excluded:
                yield d
            if rule.count is not None and emitted >= rule.count:
                return


def between(rule, start, lo, hi, exdates=()):
    """Occurrences d with lo <= d <= hi."""
    out = []
    for d in occurrences(rule, start, exdates):
        if d > hi:
            break
        if d >= lo:
            out.append(d)
    return out


def first_n(rule, start, n, exdates=()):
    out = []
    for d in occurrences(rule, start, exdates):
        if len(out) == n:
            break
        out.append(d)
    return out
