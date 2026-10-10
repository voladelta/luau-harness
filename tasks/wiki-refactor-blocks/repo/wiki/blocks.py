import re

_HEADING = re.compile(r"^(#{1,6})\s+(.*\S)\s*$")


def parse(text):
    """Split markdown-lite into blocks:
    ("heading", level, text) | ("para", text) | ("list", [items]) | ("code", text)
    """
    blocks, para, items, code = [], [], [], None

    def flush():
        nonlocal para, items
        if para:
            blocks.append(("para", " ".join(para)))
            para = []
        if items:
            blocks.append(("list", items))
            items = []

    for line in text.split("\n"):
        if code is not None:
            if line.strip() == "```":
                blocks.append(("code", "\n".join(code)))
                code = None
            else:
                code.append(line)
            continue
        if line.strip() == "```":
            flush()
            code = []
            continue
        m = _HEADING.match(line)
        if m:
            flush()
            blocks.append(("heading", len(m.group(1)), m.group(2)))
        elif line.startswith("- "):
            if para:
                flush()
            items.append(line[2:].strip())
        elif not line.strip():
            flush()
        else:
            if items:
                flush()
            para.append(line.strip())
    flush()
    if code is not None:
        blocks.append(("code", "\n".join(code)))
    return blocks
