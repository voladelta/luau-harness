"""Apply parsed patches to lists of lines."""
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


def reverse(patch):
    """The patch that undoes `patch`."""
    flip = {" ": " ", "-": "+", "+": "-"}
    hunks = [Hunk(h.new_start, h.new_len, h.old_start, h.old_len, [(flip[t], x) for t, x in h.lines]) for h in patch.hunks]
    return Patch(patch.new_name, patch.old_name, hunks)
