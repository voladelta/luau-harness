import unittest

from wiki import blocks
from wiki.convert import convert
from wiki.toc import build


class T(unittest.TestCase):
    def test_ordered_list(self):
        self.assertEqual(convert("1. one\n2. **two**\n10. ten"), "<ol><li>one</li><li><strong>two</strong></li><li>ten</li></ol>")

    def test_list_boundaries(self):
        out = convert("para line\n1. a\n2. b\n- x\n- y\n3. c\n\nafter")
        self.assertEqual(out, "<p>para line</p>\n<ol><li>a</li><li>b</li></ol>\n<ul><li>x</li><li>y</li></ul>\n<ol><li>c</li></ol>\n<p>after</p>")
        self.assertEqual(convert("1. a\n# H\n2. b"), '<ol><li>a</li></ol>\n<h1 id="h">H</h1>\n<ol><li>b</li></ol>')

    def test_toc_and_unique_ids(self):
        out = convert("# Intro\n\n[TOC]\n\n## Intro\n\n1. one\n2. two\n- x\n\n## `Code` Intro\n")
        self.assertEqual(out, "\n".join([
            '<h1 id="intro">Intro</h1>',
            '<nav class="toc"><ul><li><a href="#intro">Intro</a></li><li><a href="#intro-2">Intro</a></li>'
            '<li><a href="#code-intro"><code>Code</code> Intro</a></li></ul></nav>',
            '<h2 id="intro-2">Intro</h2>',
            "<ol><li>one</li><li>two</li></ol>",
            "<ul><li>x</li></ul>",
            '<h2 id="code-intro"><code>Code</code> Intro</h2>',
        ]))

    def test_toc_without_headings(self):
        self.assertEqual(convert("[TOC]\n\ntext"), '<nav class="toc"></nav>\n<p>text</p>')

    def test_build_unique(self):
        got = build(blocks.parse("# A\n## A\n### A\n## B"))
        self.assertEqual([s for _, _, s in got], ["a", "a-2", "a-3", "b"])
        self.assertEqual(got[0][:2], (1, "A"))
