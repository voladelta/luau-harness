"""Expand a Rule into concrete dates.

Each frequency walks periods (days, weeks, months or years, INTERVAL apart) starting at the period that contains
DTSTART. The BY* parts generate the candidate dates inside each period, or only filter them when the frequency is
finer than the part (BYMONTH under MONTHLY, BYDAY under DAILY, ...). Candidates before DTSTART are dropped.
"""
from datetime import date, timedelta

from .dates import add_months, days_in_month, make_date, month_days, week_start

HORIZON_YEARS = 400  # stop scanning a rule whose dates can never (or no longer) occur


def monthday_match(d, monthdays):
    last = days_in_month(d.year, d.month)
    return any(n == d.day or (n < 0 and last + n + 1 == d.day) for n in monthdays)


def weekdays(rule):
    return {wd for _, wd in rule.byday}


def select(rule, days):
    """Dates of one scope (a month, or a whole year) matching BYMONTHDAY and BYDAY. Ordinals count within the scope."""
    out = days
    if rule.bymonthday:
        out = [d for d in out if monthday_match(d, rule.bymonthday)]
    if rule.byday:
        nth = {}
        for d in days:
            nth.setdefault(d.weekday(), []).append(d)
        allowed = set()
        for n, wd in rule.byday:
            same = nth.get(wd, [])
            if n == 0:
                allowed.update(same)
            elif n <= len(same) and -n <= len(same):
                allowed.add(same[n - 1] if n > 0 else same[n])
        out = [d for d in out if d in allowed]
    return out


def month_candidates(rule, year, month, start):
    """Candidate dates of one month for MONTHLY and YEARLY rules, sorted."""
    if rule.bymonthday or rule.byday:
        return select(rule, month_days(year, month))
    d = make_date(year, month, start.day)
    return [d] if d is not None else []


def year_candidates(rule, year, start):
    if rule.bymonth:
        return [d for m in sorted(set(rule.bymonth)) for d in month_candidates(rule, year, m, start)]
    if rule.byday:  # BYDAY without BYMONTH: the whole year is the scope
        return select(rule, [d for m in range(1, 13) for d in month_days(year, m)])
    return month_candidates(rule, year, start.month, start)


def setpos(rule, candidates):
    if not rule.bysetpos:
        return candidates
    n = len(candidates)
    picked = {candidates[p - 1 if p > 0 else p] for p in rule.bysetpos if -n <= p <= n and p != 0}
    return sorted(picked)


def periods(rule, start):
    """Yield (first day of period, sorted candidate dates) forever."""
    if rule.freq == "DAILY":
        d = start
        while True:
            yield d, [d]
            d += timedelta(days=rule.interval)
    elif rule.freq == "WEEKLY":
        ws = week_start(start)
        days = sorted(weekdays(rule)) if rule.byday else [start.weekday()]
        while True:
            yield ws, [ws + timedelta(days=w) for w in days]
            ws += timedelta(weeks=rule.interval)
    elif rule.freq == "MONTHLY":
        i = 0
        while True:
            y, m = add_months(start.year, start.month, i * rule.interval)
            yield date(y, m, 1), setpos(rule, month_candidates(rule, y, m, start))
            i += 1
    else:  # YEARLY
        y = start.year
        while True:
            yield date(y, 1, 1), setpos(rule, year_candidates(rule, y, start))
            y += rule.interval


def keep(rule, d):
    """BY* parts that only filter at this frequency."""
    if rule.bymonth and d.month not in rule.bymonth:
        return False
    if rule.freq == "DAILY" and rule.byday and d.weekday() not in weekdays(rule):
        return False
    if rule.freq in ("DAILY", "WEEKLY") and rule.bymonthday and not monthday_match(d, rule.bymonthday):
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
