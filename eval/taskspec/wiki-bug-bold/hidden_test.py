import unittest

from wiki.convert import convert
from wiki.inline import render


class T(unittest.TestCase):
    def test_two_bolds(self):
        self.assertEqual(convert("**a** and **b**"), "<p><strong>a</strong> and <strong>b</strong></p>")

    def test_em_and_bold(self):
        self.assertEqual(render("*x* and **y**"), "<em>x</em> and <strong>y</strong>")

    def test_bold_in_list_and_code_protected(self):
        self.assertEqual(convert("- **p** q **r**"), "<ul><li><strong>p</strong> q <strong>r</strong></li></ul>")
        self.assertEqual(render("`**a**` **b**"), "<code>**a**</code> <strong>b</strong>")
