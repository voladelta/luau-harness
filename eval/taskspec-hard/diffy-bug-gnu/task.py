DOMAIN = "diffy"
MUTATIONS = [
    ("diffy/unified.py", '''    """Hunk header range from a 0-based start: "start,length" with a 1-based start, or just "start" when the
    length is 1. An empty range names the line before it (0 at the top of the file)."""
    if length == 1:
        return f"{start + 1}"
    if length == 0:
        return f"{start},0"
    return f"{start + 1},{length}"''', '''    """Hunk header range from a 0-based start: "start,length" with a 1-based start, or just "start" when the
    length is 1."""
    if length == 1:
        return f"{start + 1}"
    return f"{start + 1},{length}"'''),
    ("diffy/parse.py", '''def to_index(number, length):
    """Header line number -> 0-based start. An empty range names the line *before* it."""
    return number - 1 if length > 0 else number''', '''def to_index(number, length):
    """Header line number (1-based) -> 0-based start."""
    return number - 1'''),
    ("diffy/hunks.py", "        if k - last - 1 <= 2 * context:", "        if k - last - 1 < 2 * context:"),
]
