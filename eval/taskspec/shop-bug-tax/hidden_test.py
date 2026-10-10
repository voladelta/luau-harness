import unittest

from shop.models import Cart, Item
from shop.pricing import total


def cart(coupon=None, *items):
    c = Cart(coupon=coupon)
    for i in items:
        c.add(i)
    return c


class T(unittest.TestCase):
    def test_tax_after_percent_coupon(self):
        t = total(cart("SAVE10", Item("a", "A", 25.0, 2)), "CA")
        self.assertEqual(t, {"subtotal": 50.0, "discount": 5.0, "tax": 3.26, "total": 48.26})

    def test_tax_after_fixed_coupon(self):
        t = total(cart("FIVEOFF", Item("a", "A", 40.0, 1)), "NY")
        self.assertEqual((t["tax"], t["total"]), (1.4, 36.4))

    def test_no_coupon(self):
        t = total(cart(None, Item("a", "A", 10.0, 1)), "NY")
        self.assertEqual((t["discount"], t["tax"], t["total"]), (0.0, 0.4, 10.4))
