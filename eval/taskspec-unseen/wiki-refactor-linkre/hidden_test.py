import unittest

from wiki import inline, links
from wiki.convert import convert


class T(unittest.TestCase):
    def test_shared_pattern(self):
        self.assertTrue(hasattr(links.LINK_RE, "findall"))
        self.assertEqual(links.LINK_RE.findall("[a](http://x) [b](y)"), [("a", "http://x"), ("b", "y")])
        self.assertFalse(hasattr(inline, "_LINK"))

    def test_render_link(self):
        self.assertEqual(links.render_link("lbl", "http://u"), '<a href="http://u">lbl</a>')

    def test_behaviour_unchanged(self):
        self.assertEqual(links.extract("see [x](http://a) and [y](b)"), [("x", "http://a"), ("y", "b")])
        self.assertEqual(inline.render("go [**here**](/p) now"), 'go <a href="/p"><strong>here</strong></a> now')
        self.assertEqual(convert("- [a](b)\n\n`[c](d)`"), '<ul><li><a href="b">a</a></li></ul>\n<p><code>[c](d)</code></p>')
