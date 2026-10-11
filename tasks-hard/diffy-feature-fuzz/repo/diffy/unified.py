"""Unified diff output, compatible with GNU diff -u and patch(1)."""
from .hunks import make_hunks
from .lcs import diff


def format_range(start, length):
    """Hunk header range from a 0-based start: "start,length" with a 1-based start, or just "start" when the
    length is 1. An empty range names the line before it (0 at the top of the file)."""
    if length == 1:
        return f"{start + 1}"
    if length == 0:
        return f"{start},0"
    return f"{start + 1},{length}"


def format_hunk(h):
    out = [f"@@ -{format_range(h.old_start, h.old_len)} +{format_range(h.new_start, h.new_len)} @@"]
    out += [tag + text for tag, text in h.lines]
    return out


def unified(a, b, old_name="a", new_name="b", context=3):
    """Unified diff of two lists of lines (without newlines). Empty string when they are equal."""
    hunks = make_hunks(diff(a, b), context)
    if not hunks:
        return ""
    out = [f"--- {old_name}", f"+++ {new_name}"]
    for h in hunks:
        out += format_hunk(h)
    return "\n".join(out) + "\n"
