"""Apply parsed patches to lists of lines."""
from dataclasses import dataclass, field

from .hunks import Hunk
from .parse import Patch


class PatchError(Exception):
    def __init__(self, index, message):
        super().__init__(message)
        self.index = index  # 1-based hunk number


def apply(patch, lines):
    """Strict application: every hunk's old side must match the file exactly at the position its header gives."""
    out, pos = [], 0
    for i, h in enumerate(patch.hunks, 1):
        before = h.before()
        if h.old_start < pos or lines[h.old_start:h.old_start + len(before)] != before:
            raise PatchError(i, f"hunk #{i} does not match at line {h.old_start + 1}")
        out += lines[pos:h.old_start]
        out += h.after()
        pos = h.old_start + len(before)
    return out + lines[pos:]


@dataclass
class HunkResult:
    status: str  # "applied" or "failed"
    line: int  # 1-based line in the original file where the hunk's old side starts (expected position if failed)
    offset: int = 0
    fuzz: int = 0


@dataclass
class ApplyResult:
    lines: list
    hunks: list = field(default_factory=list)

    @property
    def failed(self):
        return [i for i, r in enumerate(self.hunks) if r.status == "failed"]


def _context(h):
    """(leading, trailing) number of context lines around the hunk's changes."""
    tags = [t for t, _ in h.lines]
    lead = next((i for i, t in enumerate(tags) if t != " "), len(tags))
    trail = next((i for i, t in enumerate(reversed(tags)) if t != " "), len(tags))
    return lead, trail


def apply_fuzzy(patch, lines, fuzz=2):
    """Apply like GNU patch: search for each hunk near its expected position, then retry ignoring up to `fuzz`
    context lines at each end. Failed hunks are skipped; the rest still apply."""
    if fuzz < 0:
        raise ValueError("fuzz must be >= 0")
    n = len(lines)
    edits, results = [], []
    last_offset = floor = 0
    for h in patch.hunks:
        before, after = h.before(), h.after()
        lead_ctx, trail_ctx = _context(h)
        p = h.old_start + last_offset
        found = None
        for f in range(fuzz + 1):
            lead, trail = min(f, lead_ctx), min(f, trail_ctx)
            pattern = before[lead:len(before) - trail]
            # q = where the full old side would start; the matched part must stay after the previous hunk and in the file
            starts = range(max(floor, 0) - lead, n - len(before) + trail + 1)
            for q in sorted(starts, key=lambda q: (abs(q - p), q < p)):  # nearest first, later before earlier
                if lines[q + lead:q + lead + len(pattern)] == pattern:
                    found = (q, f, lead, trail)
                    break
            if found:
                break
        if found is None:
            results.append(HunkResult("failed", p + 1))
            continue
        q, f, lead, trail = found
        start, end = q + lead, q + len(before) - trail
        edits.append((start, end, after[lead:len(after) - trail]))
        results.append(HunkResult("applied", q + 1, q - h.old_start, f))
        last_offset, floor = q - h.old_start, end
    out, pos = [], 0
    for start, end, new in edits:
        out += lines[pos:start] + new
        pos = end
    return ApplyResult(out + lines[pos:], results)


def report(result):
    """GNU patch style messages, one per hunk that did not apply cleanly at its stated position."""
    out = []
    for i, r in enumerate(result.hunks, 1):
        if r.status == "failed":
            out.append(f"Hunk #{i} FAILED at {r.line}.")
        elif r.offset or r.fuzz:
            msg = f"Hunk #{i} succeeded at {r.line}"
            if r.fuzz:
                msg += f" with fuzz {r.fuzz}"
            if r.offset:
                msg += f" (offset {r.offset} line{'' if abs(r.offset) == 1 else 's'})"
            out.append(msg + ".")
    return out


def reverse(patch):
    """The patch that undoes `patch`."""
    flip = {" ": " ", "-": "+", "+": "-"}
    hunks = [Hunk(h.new_start, h.new_len, h.old_start, h.old_len, [(flip[t], x) for t, x in h.lines]) for h in patch.hunks]
    return Patch(patch.new_name, patch.old_name, hunks)
