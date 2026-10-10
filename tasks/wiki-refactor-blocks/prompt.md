This is a small Python markdown-to-HTML converter (`wiki/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Refactor: `blocks.parse` returns tuples tagged with strings (`("heading", 2, "Title")`, `("para", "text")`, ...) and `html.render` / `toc.build` dispatch on those strings. Replace the tuples with dataclasses defined in `wiki/blocks.py`:

- `Heading(level: int, text: str)`, `Para(text: str)`, `ListBlock(items: list)`, `Code(text: str)`.
- `blocks.parse` returns a list of these. `html.render` and `toc.build` dispatch with `isinstance` (or `match`); an object of any other type still raises `ValueError` whose message starts with `unknown block`.
- The strings `"heading"`, `"para"`, `"list"` and `"code"` (in either quote style) may not appear anywhere in `wiki/` afterwards, docstrings included.
- Output of `convert` is unchanged. Update the existing tests.
