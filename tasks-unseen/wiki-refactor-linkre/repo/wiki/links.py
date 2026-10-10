import re

_LINK = re.compile(r"\[([^\]]+)\]\(([^)\s]+)\)")


def extract(text):
    """[(label, url)] in document order."""
    return _LINK.findall(text)
