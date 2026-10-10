from .parser import LEVELS


def by_level(records, min_level):
    floor = LEVELS.index(min_level)
    return [r for r in records if LEVELS.index(r["level"]) >= floor]


def by_component(records, component):
    return [r for r in records if r["component"] == component]


def between(records, start=None, end=None):
    """start inclusive, end exclusive; either may be None."""
    return [r for r in records if (start is None or r["ts"] >= start) and (end is None or r["ts"] < end)]


def has_field(records, name):
    return [r for r in records if name in r["fields"]]
