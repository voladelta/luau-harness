# AGENTS.md

## Comments

Ban explanatory comments. Code says what it does through names and structure; the why goes in the commit message.

Allowed, because something reads them:

- the first line of an extension in `lua/ext/` or `.hx/ext/`, which the system prompt shows
- `lua/overlay.luau`, the template `hx --init` writes for users
- directives: `--!strict`, `NOLINT`, shebangs, `/*name*/` for unnamed parameters, and the closers clang-format adds (`// namespace hx`)
- a Python module docstring that the script prints as its usage

## Tests

Run what the change touches, not the whole suite.

- While working: `ctest --preset release -E agent` (the Luau tests, seconds), or one end-to-end test with
  `python3 tests/agent_test.py build/hx AgentTest.test_name`.
- Once, before push: the full `ctest --preset release`.
- Before/after probes for a change are fine to write and delete. Do not delete or weaken an existing test to get
  green or save time; the wall fails open quietly, and only a kept test catches it.
