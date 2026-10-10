import html as _html

from . import inline
from .toc import slug


def render(blocks):
    out = []
    for b in blocks:
        kind = b[0]
        if kind == "heading":
            out.append(f'<h{b[1]} id="{slug(b[2])}">{inline.render(b[2])}</h{b[1]}>')
        elif kind == "para":
            out.append(f"<p>{inline.render(b[1])}</p>")
        elif kind == "list":
            out.append("<ul>" + "".join(f"<li>{inline.render(i)}</li>" for i in b[1]) + "</ul>")
        elif kind == "code":
            out.append(f"<pre><code>{_html.escape(b[1], quote=False)}</code></pre>")
        else:
            raise ValueError(f"unknown block: {kind}")
    return "\n".join(out)
