DOMAIN = "wiki"
CHECK = r"""n=$(grep -rnF '\]\(' wiki/ --include=*.py | wc -l); if [ "$n" -ne 1 ]; then echo "link pattern defined $n times"; fail=1; fi
if grep -rn '<a href=' wiki/ --include=*.py | grep -v '^wiki/links.py'; then echo "anchor markup outside links.py"; fail=1; fi"""


def solve(h):
    links = (h.ws / "wiki/links.py").read_text().replace("_LINK", "LINK_RE")
    links += "\n\ndef render_link(label, url):\n    return f'<a href=\"{url}\">{label}</a>'\n"
    h.write("wiki/links.py", links)
    out = []
    for line in (h.ws / "wiki/inline.py").read_text().split("\n"):
        if line.startswith("_LINK = "):
            continue
        if "_LINK.sub(" in line:
            line = "    text = LINK_RE.sub(lambda m: render_link(m.group(1), m.group(2)), text)"
        out.append(line)
    src = "\n".join(out).replace("import re\n", "import re\n\nfrom .links import LINK_RE, render_link\n", 1)
    assert "_LINK" not in src
    h.write("wiki/inline.py", src)
