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
    byday: list = field(default_factory=list)  # weekday ints, 0 = Monday
    bymonthday: list = field(default_factory=list)
    bymonth: list = field(default_factory=list)

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
    if code not in WEEKDAY_CODES:
        raise ValueError(f"BYDAY: bad weekday {code!r}")
    return WEEKDAY_CODES.index(code)


def validate(rule):
    if rule.freq not in FREQS:
        raise ValueError(f"FREQ: unknown frequency {rule.freq!r}")
    if rule.interval < 1:
        raise ValueError("INTERVAL must be at least 1")
    if rule.count is not None and rule.count < 1:
        raise ValueError("COUNT must be at least 1")
    if rule.count is not None and rule.until is not None:
        raise ValueError("COUNT and UNTIL cannot be combined")


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
            kw["bymonthday"] = _ints(key, value, 1, 31)
        elif key == "BYMONTH":
            kw["bymonth"] = _ints(key, value, 1, 12)
        else:
            raise ValueError(f"unsupported rule part {key}")
    return Rule(**kw)


def format_rule(rule):
    """Canonical text: FREQ, INTERVAL (if not 1), BYMONTH, BYMONTHDAY, BYDAY, COUNT, UNTIL."""
    out = [f"FREQ={rule.freq}"]
    if rule.interval != 1:
        out.append(f"INTERVAL={rule.interval}")
    if rule.bymonth:
        out.append("BYMONTH=" + ",".join(map(str, rule.bymonth)))
    if rule.bymonthday:
        out.append("BYMONTHDAY=" + ",".join(map(str, rule.bymonthday)))
    if rule.byday:
        out.append("BYDAY=" + ",".join(WEEKDAY_CODES[d] for d in rule.byday))
    if rule.count is not None:
        out.append(f"COUNT={rule.count}")
    if rule.until is not None:
        out.append(f"UNTIL={rule.until:%Y%m%d}")
    return ";".join(out)
