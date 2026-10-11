"""Group diff operations into hunks."""
from dataclasses import dataclass, field


@dataclass
class Hunk:
    old_start: int  # 0-based index in the old file of the first line the hunk covers
    old_len: int
    new_start: int  # 0-based index in the new file
    new_len: int
    lines: list = field(default_factory=list)  # (tag, text), tag in " ", "-", "+"

    def before(self):
        return [x for t, x in self.lines if t != "+"]

    def after(self):
        return [x for t, x in self.lines if t != "-"]


def positions(ops):
    """Old and new line index at each op."""
    old_at, new_at, o, n = [], [], 0, 0
    for tag, _ in ops:
        old_at.append(o)
        new_at.append(n)
        if tag != "+":
            o += 1
        if tag != "-":
            n += 1
    return old_at, new_at


def make_hunks(ops, context=3):
    """Hunks with up to `context` unchanged lines around each change. Changes separated by at most 2 * context
    unchanged lines share a hunk, since their context lines would touch or overlap."""
    changes = [k for k, (tag, _) in enumerate(ops) if tag != " "]
    if not changes:
        return []
    groups = []
    first = last = changes[0]
    for k in changes[1:]:
        if k - last - 1 < 2 * context:
            last = k
        else:
            groups.append((first, last))
            first = last = k
    groups.append((first, last))
    old_at, new_at = positions(ops)
    hunks = []
    for first, last in groups:
        lo, hi = max(0, first - context), min(len(ops), last + context + 1)
        lines = ops[lo:hi]
        hunks.append(Hunk(old_at[lo], sum(t != "+" for t, _ in lines), new_at[lo], sum(t != "-" for t, _ in lines), list(lines)))
    return hunks
