"""Plain-English summaries of rules, e.g. "every 2 weeks on Monday and Wednesday, 10 times"."""
from .dates import MONTH_NAMES, WEEKDAY_NAMES

_ADVERB = {"DAILY": "daily", "WEEKLY": "weekly", "MONTHLY": "monthly", "YEARLY": "yearly"}
_UNIT = {"DAILY": "day", "WEEKLY": "week", "MONTHLY": "month", "YEARLY": "year"}


def join(items):
    items = list(items)
    if len(items) <= 1:
        return "".join(items)
    return ", ".join(items[:-1]) + " and " + items[-1]


def describe(rule):
    parts = [_ADVERB[rule.freq] if rule.interval == 1 else f"every {rule.interval} {_UNIT[rule.freq]}s"]
    if rule.bymonth:
        parts.append("in " + join(MONTH_NAMES[m - 1] for m in sorted(set(rule.bymonth))))
    if rule.bymonthday:
        parts.append("on day " + join(str(n) for n in sorted(set(rule.bymonthday))))
    if rule.byday:
        parts.append(("if a " if rule.bymonthday else "on ") + join(WEEKDAY_NAMES[d] for d in sorted(set(rule.byday))))
    text = " ".join(parts)
    if rule.count is not None:
        text += f", {rule.count} time{'s' if rule.count != 1 else ''}"
    if rule.until is not None:
        text += f", until {rule.until:%Y-%m-%d}"
    return text
