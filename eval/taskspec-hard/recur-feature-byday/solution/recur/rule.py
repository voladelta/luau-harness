"""Recurrence rules: a subset of RFC 5545 RRULE, e.g. "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=10"."""
from dataclasses import dataclass, field

from .dates import WEEKDAY_CODES, parse_date

FREQS = ("DAILY", "WEEKLY", "MONTHLY", "YEARLY")


@dataclass
class Rule:
    freq: str
    interval: int = 1
    count: int | None = None
    until: object = None  # datetime.date, inclusive
    byday: list = field(default_factory=list)  # (ordinal, weekday) pairs; ordinal 0 = every such weekday
    bymonthday: list = field(default_factory=list)  # negative = from the end of the month
    bymonth: list = field(default_factory=list)
    bysetpos: list = field(default_factory=list)

    def __post_init__(self):
        validate(self)


def _ints(key, text, lo, hi):
    out = []
    for part in text.split(","):
        try:
            n = int(part)
        except ValueError:
            raise ValueError(f"{key}: bad number {part!r}") from None
        if not lo <= n <= hi:
            raise ValueError(f"{key}: {n} out of range {lo}..{hi}")
        out.append(n)
    return out


def _weekday(code):
    """'MO' -> (0, 0), '-1FR' -> (-1, 4), '+2TU' -> (2, 1)."""
    num, wd = code[:-2], code[-2:]
    if wd not in WEEKDAY_CODES:
        raise ValueError(f"BYDAY: bad weekday {code!r}")
    n = 0
    if num:
        try:
            n = int(num)
        except ValueError:
            raise ValueError(f"BYDAY: bad ordinal {code!r}") from None
        if not 1 <= abs(n) <= 53:
            raise ValueError(f"BYDAY: ordinal out of range in {code!r}")
    return n, WEEKDAY_CODES.index(wd)


def _nonzero(key, values):
    if 0 in values:
        raise ValueError(f"{key}: 0 is not allowed")
    return values


def validate(rule):
    if rule.freq not in FREQS:
        raise ValueError(f"FREQ: unknown frequency {rule.freq!r}")
    if rule.interval < 1:
        raise ValueError("INTERVAL must be at least 1")
    if rule.count is not None and rule.count < 1:
        raise ValueError("COUNT must be at least 1")
    if rule.count is not None and rule.until is not None:
        raise ValueError("COUNT and UNTIL cannot be combined")
    if any(n for n, _ in rule.byday) and rule.freq not in ("MONTHLY", "YEARLY"):
        raise ValueError("BYDAY ordinals require FREQ=MONTHLY or FREQ=YEARLY")
    if rule.bysetpos:
        if rule.freq not in ("MONTHLY", "YEARLY"):
            raise ValueError("BYSETPOS requires FREQ=MONTHLY or FREQ=YEARLY")
        if not (rule.byday or rule.bymonthday):
            raise ValueError("BYSETPOS requires BYDAY or BYMONTHDAY")


def parse(text):
    parts = {}
    for item in text.strip().split(";"):
        if not item:
            continue
        key, sep, value = item.partition("=")
        key = key.strip().upper()
        if not sep or not value:
            raise ValueError(f"bad rule part {item!r}")
        if key in parts:
            raise ValueError(f"{key} given twice")
        parts[key] = value.strip().upper()
    if "FREQ" not in parts:
        raise ValueError("FREQ is required")
    kw = {"freq": parts.pop("FREQ")}
    for key, value in parts.items():
        if key == "INTERVAL":
            kw["interval"] = _ints(key, value, 1, 10**6)[0]
        elif key == "COUNT":
            kw["count"] = _ints(key, value, 1, 10**6)[0]
        elif key == "UNTIL":
            kw["until"] = parse_date(value)
        elif key == "BYDAY":
            kw["byday"] = [_weekday(c) for c in value.split(",")]
        elif key == "BYMONTHDAY":
            kw["bymonthday"] = _nonzero(key, _ints(key, value, -31, 31))
        elif key == "BYSETPOS":
            kw["bysetpos"] = _nonzero(key, _ints(key, value, -366, 366))
        elif key == "BYMONTH":
            kw["bymonth"] = _ints(key, value, 1, 12)
        else:
            raise ValueError(f"unsupported rule part {key}")
    return Rule(**kw)


def format_rule(rule):
    """Canonical text: FREQ, INTERVAL (if not 1), BYMONTH, BYMONTHDAY, BYDAY, BYSETPOS, COUNT, UNTIL."""
    out = [f"FREQ={rule.freq}"]
    if rule.interval != 1:
        out.append(f"INTERVAL={rule.interval}")
    if rule.bymonth:
        out.append("BYMONTH=" + ",".join(map(str, rule.bymonth)))
    if rule.bymonthday:
        out.append("BYMONTHDAY=" + ",".join(map(str, rule.bymonthday)))
    if rule.byday:
        out.append("BYDAY=" + ",".join(f"{n or ''}{WEEKDAY_CODES[d]}" for n, d in rule.byday))
    if rule.bysetpos:
        out.append("BYSETPOS=" + ",".join(map(str, rule.bysetpos)))
    if rule.count is not None:
        out.append(f"COUNT={rule.count}")
    if rule.until is not None:
        out.append(f"UNTIL={rule.until:%Y%m%d}")
    return ";".join(out)
