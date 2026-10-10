This is a small Python markdown-to-HTML converter (`wiki/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Refactor: the regular expression for `[label](url)` links is defined twice, once in `wiki/inline.py` and once in `wiki/links.py`. Make it a single definition:

- `wiki/links.py` defines a public compiled pattern `LINK_RE`, and `links.extract` uses it.
- `wiki/inline.py` imports and uses `links.LINK_RE` (it must not define its own link pattern). Rendering of links stays exactly as is.
- Also add `links.render_link(label, url)` returning `'<a href="URL">LABEL</a>'`, and make `inline.render` use it for every link, so the anchor markup is defined in one place too.
- Nothing else changes. Update the existing tests.
