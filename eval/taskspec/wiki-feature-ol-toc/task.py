DOMAIN = "wiki"

def solve(h):
    h.edit("wiki/blocks.py", '_HEADING = re.compile(r"^(#{1,6})\\s+(.*\\S)\\s*$")', '_HEADING = re.compile(r"^(#{1,6})\\s+(.*\\S)\\s*$")\n_OLI = re.compile(r"^\\d+\\.\\s+(.*\\S)\\s*$")')
    h.edit("wiki/blocks.py", "blocks, para, items, code = [], [], [], None", "blocks, para, items, oitems, code = [], [], [], [], None")
    h.edit("wiki/blocks.py", "nonlocal para, items\n", "nonlocal para, items, oitems\n")
    h.edit("wiki/blocks.py", "            items = []\n", "            items = []\n        if oitems:\n            blocks.append((\"olist\", oitems))\n            oitems = []\n")
    h.edit("wiki/blocks.py", '''            if para:
                flush()
            items.append(line[2:].strip())''', '''            if para or oitems:
                flush()
            items.append(line[2:].strip())
        elif _OLI.match(line):
            if para or items:
                flush()
            oitems.append(_OLI.match(line).group(1))
        elif line.strip() == "[TOC]":
            flush()
            blocks.append(("toc",))''')
    h.edit("wiki/blocks.py", "            if items:\n                flush()\n            para.append", "            if items or oitems:\n                flush()\n            para.append")
    h.write("wiki/toc.py", '''import re


def slug(text):
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def build(blocks):
    """[(level, text, unique_slug)] for every heading block, in order."""
    seen, out = {}, []
    for b in blocks:
        if b[0] == "heading":
            s = slug(b[2])
            seen[s] = seen.get(s, 0) + 1
            out.append((b[1], b[2], s if seen[s] == 1 else f"{s}-{seen[s]}"))
    return out
''')
    h.edit("wiki/html.py", "from . import inline\nfrom .toc import slug\n", "from . import inline, toc\n")
    h.edit("wiki/html.py", "    out = []\n    for b in blocks:", "    out = []\n    entries = toc.build(blocks)\n    heads = iter(entries)\n    for b in blocks:")
    h.edit("wiki/html.py", '''            out.append(f'<h{b[1]} id="{slug(b[2])}">{inline.render(b[2])}</h{b[1]}>')''', '''            _, _, sl = next(heads)
            out.append(f'<h{b[1]} id="{sl}">{inline.render(b[2])}</h{b[1]}>')''')
    h.edit("wiki/html.py", '        elif kind == "code":', '''        elif kind == "olist":
            out.append("<ol>" + "".join(f"<li>{inline.render(i)}</li>" for i in b[1]) + "</ol>")
        elif kind == "toc":
            if entries:
                items = "".join(f'<li><a href="#{s}">{inline.render(t)}</a></li>' for _, t, s in entries)
                out.append(f'<nav class="toc"><ul>{items}</ul></nav>')
            else:
                out.append('<nav class="toc"></nav>')
        elif kind == "code":''')
