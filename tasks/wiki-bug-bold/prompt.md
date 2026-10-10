This is a small Python markdown-to-HTML converter (`wiki/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Bug report: when a paragraph contains two bold phrases, e.g. `**a** and **b**`, the output has one big bold region covering everything from the first `**` to the last `**`, including the text in between, instead of two separate bold phrases.

Find and fix the bug. Don't change any public function names or signatures.
