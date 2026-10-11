"""Events with optional recurrence, and a calendar that lists them over a date range."""
from dataclasses import dataclass, field
from datetime import date

from .dates import shift_months
from .expand import between, occurrences
from .rule import Rule, parse


@dataclass
class Event:
    name: str
    start: date
    rule: Rule | None = None  # None = a one-off event
    exdates: set = field(default_factory=set)

    @classmethod
    def recurring(cls, name, start, rule_text, exdates=()):
        return cls(name, start, parse(rule_text), set(exdates))

    def dates_between(self, lo, hi):
        if self.rule is None:
            return [self.start] if lo <= self.start <= hi and self.start not in self.exdates else []
        return between(self.rule, self.start, lo, hi, self.exdates)

    def next_after(self, d):
        """First occurrence strictly after d, or None."""
        if self.rule is None:
            return self.start if self.start > d and self.start not in self.exdates else None
        for x in occurrences(self.rule, self.start, self.exdates):
            if x > d:
                return x
        return None

    def postpone(self, months):
        """Move the whole series n months later; the start date clamps to the end of a shorter month."""
        self.start = shift_months(self.start, months)


class Calendar:
    def __init__(self):
        self.events = []

    def add(self, event):
        if any(e.name == event.name for e in self.events):
            raise ValueError(f"duplicate event {event.name!r}")
        self.events.append(event)
        return event

    def get(self, name):
        for e in self.events:
            if e.name == name:
                return e
        raise KeyError(name)

    def between(self, lo, hi):
        """[(date, event name)] sorted by date, then by the order events were added."""
        out = [(d, i, e.name) for i, e in enumerate(self.events) for d in e.dates_between(lo, hi)]
        return [(d, name) for d, _, name in sorted(out)]

    def agenda(self, lo, hi):
        lines = []
        for d, name in self.between(lo, hi):
            lines.append(f"{d:%a %Y-%m-%d}  {name}")
        return "\n".join(lines)
