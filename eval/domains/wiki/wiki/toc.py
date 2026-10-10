import re


def slug(text):
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def build(blocks):
    """[(level, text, slug)] for every heading block, in order."""
    return [(b[1], b[2], slug(b[2])) for b in blocks if b[0] == "heading"]
