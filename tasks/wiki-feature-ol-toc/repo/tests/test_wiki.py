import unittest

from wiki.convert import convert
from wiki.links import extract
from wiki.toc import build, slug
from wiki import blocks


class WikiTests(unittest.TestCase):
    def test_convert(self):
        out = convert("# Hi There\n\nsome *em* and `x*y*` text\n\n- a\n- b\n")
        self.assertIn('<h1 id="hi-there">Hi There</h1>', out)
        self.assertIn("<em>em</em>", out)
        self.assertIn("<code>x*y*</code>", out)
        self.assertIn("<ul><li>a</li><li>b</li></ul>", out)

    def test_code_and_links(self):
        self.assertIn("<pre><code>a &lt; b</code></pre>", convert("```\na < b\n```"))
        self.assertEqual(extract("see [x](http://a) and [y](b)"), [("x", "http://a"), ("y", "b")])

    def test_toc(self):
        self.assertEqual(build(blocks.parse("# A\n## B c")), [(1, "A", "a"), (2, "B c", "b-c")])
        self.assertEqual(slug("Hello, World!"), "hello-world")


if __name__ == "__main__":
    unittest.main()
