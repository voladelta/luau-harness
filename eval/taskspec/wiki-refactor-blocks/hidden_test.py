import dataclasses, unittest

from wiki import blocks
from wiki.blocks import Code, Heading, ListBlock, Para
from wiki.convert import convert
from wiki.html import render
from wiki.toc import build

DOC = "# Title\n\nsome *text* here\nmore\n\n- a\n- b **c**\n\n```\nx < y\n```\n\n## Sub Title\n"


class T(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(blocks.parse(DOC), [Heading(1, "Title"), Para("some *text* here more"), ListBlock(["a", "b **c**"]), Code("x < y"), Heading(2, "Sub Title")])
        for cls in (Heading, Para, ListBlock, Code):
            self.assertTrue(dataclasses.is_dataclass(cls))
        h = Heading(2, "T")
        self.assertEqual((h.level, h.text), (2, "T"))
        self.assertNotIsInstance(h, tuple)

    def test_render(self):
        self.assertEqual(convert(DOC), "\n".join([
            '<h1 id="title">Title</h1>', "<p>some <em>text</em> here more</p>", "<ul><li>a</li><li>b <strong>c</strong></li></ul>",
            "<pre><code>x &lt; y</code></pre>", '<h2 id="sub-title">Sub Title</h2>']))
        self.assertEqual(render([Heading(3, "A b"), Para("p")]), '<h3 id="a-b">A b</h3>\n<p>p</p>')

    def test_unknown_block(self):
        for bad in (("para", "x"), object(), None):
            with self.assertRaisesRegex(ValueError, "^unknown block"):
                render([bad])

    def test_toc(self):
        self.assertEqual(build(blocks.parse(DOC)), [(1, "Title", "title"), (2, "Sub Title", "sub-title")])
        self.assertEqual(build([Para("x"), Heading(2, "Q?")]), [(2, "Q?", "q")])
