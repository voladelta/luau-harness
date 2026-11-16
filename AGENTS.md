# AGENTS.md

## Comments

Ban explanatory comments. Code says what it does through names and structure; the why goes in the commit message.

Allowed, because something reads them:

- the first line of an extension in `lua/ext/` or `.hx/ext/`, which the system prompt shows
- `lua/overlay.luau`, the template `hx --init` writes for users
- directives: `--!strict`, `NOLINT`, shebangs, `/*name*/` for unnamed parameters, and the closers clang-format adds (`// namespace hx`)
- a Python module docstring that the script prints as its usage
