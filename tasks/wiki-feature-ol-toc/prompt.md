This is a small Python markdown-to-HTML converter (`wiki/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Add these features:

1. Ordered lists: consecutive lines that start with digits, a dot and whitespace (`1. one`) form a list rendered as `<ol><li>one</li><li>two</li></ol>` (inline markup works inside items). Like bullet lists, an ordered list ends at a blank line, a heading, or a line of a different kind, and it can directly follow a paragraph line without a blank line in between.
2. A line consisting only of `[TOC]` becomes a table of contents listing ALL headings of the document (also those after the marker), in order, rendered exactly as `<nav class="toc"><ul><li><a href="#SLUG">TEXT</a></li>...</ul></nav>` where TEXT is the inline-rendered heading text. If the document has no headings the output is `<nav class="toc"></nav>`.
3. Heading ids must be unique: when several headings have the same slug, the first keeps it and later ones get `-2`, `-3`, ... appended (`intro`, `intro-2`, `intro-3`). The ids used on the `<hN>` tags and in the TOC links must agree, and `toc.build(blocks)` must return the same unique slugs.

Update or add tests as you see fit.
