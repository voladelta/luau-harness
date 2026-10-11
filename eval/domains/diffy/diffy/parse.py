"""Parse unified diffs."""
import re
from dataclasses import dataclass, field

from .hunks import Hunk

HEADER = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")


class ParseError(ValueError):
    pass


@dataclass
class Patch:
    old_name: str
    new_name: str
    hunks: list = field(default_factory=list)


def to_index(number, length):
    """Header line number -> 0-based start. An empty range names the line *before* it."""
    return number - 1 if length > 0 else number


def parse_patch(text):
    lines = text.splitlines()
    i = 0
    while i < len(lines) and not lines[i].startswith("--- "):
        i += 1
    if i + 1 >= len(lines) or not lines[i + 1].startswith("+++ "):
        raise ParseError("missing ---/+++ header")
    patch = Patch(lines[i][4:].split("\t")[0], lines[i + 1][4:].split("\t")[0])
    i += 2
    while i < len(lines):
        m = HEADER.match(lines[i])
        if not m:
            if lines[i].startswith("--- "):
                break  # next file in a multi-file patch: not supported, stop here
            raise ParseError(f"line {i + 1}: expected a hunk header, got {lines[i]!r}")
        os_, ol, ns, nl = (int(g) if g is not None else 1 for g in m.groups())
        h = Hunk(to_index(os_, ol), ol, to_index(ns, nl), nl)
        i += 1
        old_left, new_left = ol, nl
        while old_left or new_left:
            if i >= len(lines):
                raise ParseError(f"hunk at line {os_} is truncated")
            line = lines[i]
            tag, body = (line[0], line[1:]) if line else (" ", "")  # some tools strip the space of empty context
            if tag == "\\":
                i += 1
                continue
            if tag not in " -+" or (tag != "+" and not old_left) or (tag != "-" and not new_left):
                raise ParseError(f"line {i + 1}: unexpected {line!r} in hunk")
            h.lines.append((tag, body))
            old_left -= tag != "+"
            new_left -= tag != "-"
            i += 1
        while i < len(lines) and lines[i].startswith("\\"):
            i += 1
        if patch.hunks and h.old_start < patch.hunks[-1].old_start + patch.hunks[-1].old_len:
            raise ParseError(f"hunk at line {os_} overlaps the previous one")
        patch.hunks.append(h)
    return patch
