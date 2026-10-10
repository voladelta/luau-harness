DOMAIN = "wiki"
CHECK = '''if grep -rnE "[\\"'](heading|para|list|code)[\\"']" wiki/ --include=*.py; then echo "block tag strings remain"; fail=1; fi'''

def solve(h):
    p = h.ws / "wiki/blocks.py"
    s = p.read_text()
    s = s.replace('import re\n', 'import re\nfrom dataclasses import dataclass\n', 1)
    s = s.replace('def parse(text):', '''@dataclass
class Heading:
    level: int
    text: str


@dataclass
class Para:
    text: str


@dataclass
class ListBlock:
    items: list


@dataclass
class Code:
    text: str


def parse(text):''', 1)
    s = s.replace('''    """Split markdown-lite into blocks:
    ("heading", level, text) | ("para", text) | ("list", [items]) | ("code", text)
    """''', '    """Split markdown-lite into Heading, Para, ListBlock and Code blocks."""')
    s = s.replace('("para", " ".join(para))', 'Para(" ".join(para))').replace('("list", items)', 'ListBlock(items)')
    s = s.replace('("code", "\\n".join(code))', 'Code("\\n".join(code))').replace('("heading", len(m.group(1)), m.group(2))', 'Heading(len(m.group(1)), m.group(2))')
    p.write_text(s)
    h.edit("wiki/toc.py", 'import re\n', 'import re\n\nfrom .blocks import Heading\n')
    h.edit("wiki/toc.py", '[(b[1], b[2], slug(b[2])) for b in blocks if b[0] == "heading"]', '[(b.level, b.text, slug(b.text)) for b in blocks if isinstance(b, Heading)]')
    h.write("wiki/html.py", '''import html as _html

from . import inline
from .blocks import Code, Heading, ListBlock, Para
from .toc import slug


def render(blocks):
    out = []
    for b in blocks:
        if isinstance(b, Heading):
            out.append(f'<h{b.level} id="{slug(b.text)}">{inline.render(b.text)}</h{b.level}>')
        elif isinstance(b, Para):
            out.append(f"<p>{inline.render(b.text)}</p>")
        elif isinstance(b, ListBlock):
            out.append("<ul>" + "".join(f"<li>{inline.render(i)}</li>" for i in b.items) + "</ul>")
        elif isinstance(b, Code):
            out.append(f"<pre><code>{_html.escape(b.text, quote=False)}</code></pre>")
        else:
            raise ValueError(f"unknown block: {b!r}")
    return "\\n".join(out)
''')
