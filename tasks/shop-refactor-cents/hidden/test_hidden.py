import contextlib, io, json, os, tempfile, unittest

from shop.cli import main
from shop.discounts import discount_for
from shop.fmt import money, receipt
from shop.models import Cart, Item
from shop.orders import place_order
from shop.pricing import line_total, subtotal, total
from shop.tax import RATES_BP, tax_for


def cart(coupon, *items):
    c = Cart(coupon=coupon)
    for i in items:
        c.add(i)
    return c


class T(unittest.TestCase):
    def test_model(self):
        c = cart(None, Item("a", "A", 250, 2), Item("a", "A", 250, 1))
        self.assertEqual(c.items[0].price_cents, 250)
        self.assertEqual((c.count(), subtotal(c), line_total(c.items[0])), (3, 750, 750))
        self.assertFalse(hasattr(c.items[0], "price"))

    def test_discounts(self):
        self.assertEqual(discount_for(cart("SAVE10", Item("a", "A", 1235, 1)), "SAVE10"), 124)
        self.assertEqual(discount_for(cart("SAVE10", Item("a", "A", 1234, 1)), "SAVE10"), 123)
        self.assertEqual(discount_for(cart(None, Item("a", "A", 2999, 1)), "FIVEOFF"), 0)
        self.assertEqual(discount_for(cart(None, Item("a", "A", 3000, 1)), "FIVEOFF"), 500)
        self.assertEqual(discount_for(cart(None, Item("a", "A", 1, 1)), None), 0)
        with self.assertRaises(ValueError):
            discount_for(cart(None), "NOPE")

    def test_tax(self):
        self.assertEqual(RATES_BP, {"CA": 725, "NY": 400, "OR": 0})
        self.assertEqual(tax_for(1999, "CA"), 145)
        self.assertEqual(tax_for(1999, "OR"), 0)
        with self.assertRaises(ValueError):
            tax_for(1, "ZZ")

    def test_total_types_and_values(self):
        t = total(cart("SAVE10", Item("a", "A", 2500, 2)), "CA")
        self.assertEqual(t, {"subtotal": 5000, "discount": 500, "tax": 326, "total": 4826})
        self.assertTrue(all(type(v) is int for v in t.values()))
        t = total(cart("FIVEOFF", Item("a", "A", 4000, 1)), "NY")
        self.assertEqual(t, {"subtotal": 4000, "discount": 500, "tax": 140, "total": 3640})

    def test_money_and_receipt(self):
        self.assertEqual((money(123456), money(5), money(0), money(100000)), ("$1,234.56", "$0.05", "$0.00", "$1,000.00"))
        o = place_order(cart("FIVEOFF", Item("a", "Widget", 4000, 1)), "NY")
        self.assertEqual(o["lines"][0]["total"], 4000)
        text = receipt(o)
        for want in ("1 x Widget: $40.00", "Subtotal: $40.00", "Discount: -$5.00", "Tax: $1.40", "Total: $36.40"):
            self.assertIn(want, text.split("\n"))

    def test_cli(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "c.json")
            json.dump({"items": [{"sku": "A", "name": "A", "price_cents": 1000, "qty": 2}], "coupon": None}, open(p, "w"))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(main([p, "NY"]), 0)
            self.assertIn("Total: $20.80", out.getvalue())
