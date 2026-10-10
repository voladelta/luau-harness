import html
import re

_CODE = re.compile(r"`([^`]+)`")
_BOLD = re.compile(r"\*\*(.+?)\*\*")
_EM = re.compile(r"\*(.+?)\*")
_LINK = re.compile(r"\[([^\]]+)\]\(([^)\s]+)\)")


def render(text):
    """Inline markup -> HTML. Code spans are protected from further formatting."""
    text = html.escape(text, quote=False)
    spans = []

    def stash(m):
        spans.append(f"<code>{m.group(1)}</code>")
        return f"\x00{len(spans) - 1}\x00"

    text = _CODE.sub(stash, text)
    text = _BOLD.sub(r"<strong>\1</strong>", text)
    text = _EM.sub(r"<em>\1</em>", text)
    text = _LINK.sub(r'<a href="\2">\1</a>', text)
    return re.sub(r"\x00(\d+)\x00", lambda m: spans[int(m.group(1))], text)
