import unittest

from shop.fmt import money
from shop.models import Cart, Item
from shop.orders import place_order
from shop.pricing import subtotal, total


def cart(*items, coupon=None):
    c = Cart(coupon=coupon)
    for it in items:
        c.add(it)
    return c


class ShopTests(unittest.TestCase):
    def test_merge_and_subtotal(self):
        c = cart(Item("a", "A", 2.5, 2), Item("a", "A", 2.5, 1))
        self.assertEqual(c.count(), 3)
        self.assertEqual(subtotal(c), 7.5)

    def test_total_no_coupon(self):
        t = total(cart(Item("a", "A", 10.0, 1)), "NY")
        self.assertEqual(t["total"], 10.4)

    def test_fiveoff(self):
        t = total(cart(Item("a", "A", 40.0, 1), coupon="FIVEOFF"), "OR")
        self.assertEqual((t["discount"], t["total"]), (5.0, 35.0))

    def test_order_and_money(self):
        o = place_order(cart(Item("a", "A", 1.0, 1)), "OR")
        self.assertTrue(o["id"].startswith("ORD-"))
        self.assertEqual(money(1234.5), "$1,234.50")


if __name__ == "__main__":
    unittest.main()
