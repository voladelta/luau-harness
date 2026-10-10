import unittest

from shop.fmt import money, receipt
from shop.models import Cart, Item
from shop.orders import place_order


class T(unittest.TestCase):
    def test_money(self):
        self.assertEqual(money(1234.5), "$1,234.50")
        self.assertEqual(money(12000), "$12,000.00")
        self.assertEqual(money(1234567.891), "$1,234,567.89")
        self.assertEqual(money(0.5), "$0.50")

    def test_receipt(self):
        c = Cart()
        c.add(Item("a", "Big", 1500.0, 2))
        text = receipt(place_order(c, "OR"))
        for want in ("1 x Big: $3,000.00".replace("1 x", "2 x"), "Subtotal: $3,000.00", "Total: $3,000.00"):
            self.assertIn(want, text.split("\n"))
