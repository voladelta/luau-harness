import contextlib, io, json, os, tempfile, unittest

from shop.cli import main
from shop.discounts import discount_for
from shop.fmt import receipt
from shop.models import Cart, Item
from shop.orders import place_order


def cart(coupon, *items):
    c = Cart(coupon=coupon)
    for i in items:
        c.add(i)
    return c


class T(unittest.TestCase):
    def test_discount(self):
        c = cart(None, Item("A1", "A", 4.0, 7), Item("B", "B", 1.0, 3))
        self.assertEqual(discount_for(c, "BUY2GET1:A1"), 8.0)
        self.assertEqual(discount_for(c, "BUY2GET1:B"), 1.0)
        self.assertEqual(discount_for(c, "BUY2GET1:ZZ"), 0.0)
        self.assertEqual(discount_for(cart(None, Item("A1", "A", 4.0, 2)), "BUY2GET1:A1"), 0.0)
        with self.assertRaises(ValueError):
            discount_for(c, "NOPE")
        self.assertEqual(discount_for(c, "SAVE10"), 3.1)

    def test_order_and_receipt(self):
        o = place_order(cart("BUY2GET1:A1", Item("A1", "A", 4.0, 7)), "OR")
        self.assertEqual(o["coupon"], "BUY2GET1:A1")
        self.assertEqual((o["discount"], o["total"]), (8.0, 20.0))
        self.assertIn("Discount (BUY2GET1:A1): -$8.00", receipt(o).split("\n"))
        o2 = place_order(cart(None, Item("A1", "A", 4.0, 1)), "OR")
        self.assertIsNone(o2["coupon"])
        self.assertNotIn("Discount", receipt(o2))
        o3 = place_order(cart("FIVEOFF", Item("A1", "A", 4.0, 1)), "OR")
        self.assertNotIn("Discount", receipt(o3))
        o4 = place_order(cart("FIVEOFF", Item("A1", "A", 40.0, 1)), "OR")
        self.assertIn("Discount (FIVEOFF): -$5.00", receipt(o4))

    def run_cli(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = main(argv)
        return rc, out.getvalue(), err.getvalue()

    def test_cli(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "c.json")
            json.dump({"items": [{"sku": "A1", "name": "A", "price": 40.0, "qty": 1}], "coupon": "SAVE10"}, open(p, "w"))
            rc, out, _ = self.run_cli([p, "OR"])
            self.assertEqual(rc, 0)
            self.assertIn("Discount (SAVE10): -$4.00", out)
            rc, out, _ = self.run_cli([p, "OR", "--coupon", "FIVEOFF"])
            self.assertEqual(rc, 0)
            self.assertIn("Discount (FIVEOFF): -$5.00", out)
            self.assertNotIn("SAVE10", out)
            rc, out, err = self.run_cli([p, "OR", "--bogus", "x"])
            self.assertEqual(rc, 2)
            self.assertTrue(err)
