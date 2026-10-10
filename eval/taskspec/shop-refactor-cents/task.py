DOMAIN = "shop"
CHECK = '''if grep -rnE '\\bfloat\\b|round\\(|[0-9]\\.[0-9]' shop/ --include=*.py; then echo "float arithmetic remains in shop/"; fail=1; fi'''

def solve(h):
    h.edit("shop/models.py", "    price: float\n", "    price_cents: int\n")
    h.edit("shop/models.py", "Item(item.sku, item.name, item.price, item.qty)", "Item(item.sku, item.name, item.price_cents, item.qty)")
    h.write("shop/discounts.py", '''def discount_for(cart, code):
    """Discount in integer cents for `code` on `cart`. Unknown codes raise ValueError."""
    if not code:
        return 0
    sub = sum(i.price_cents * i.qty for i in cart.items)
    if code == "SAVE10":
        return (sub * 10 + 50) // 100
    if code == "FIVEOFF":
        return 500 if sub >= 3000 else 0
    raise ValueError(f"unknown coupon: {code}")
''')
    h.write("shop/tax.py", '''RATES_BP = {"CA": 725, "NY": 400, "OR": 0}


def tax_for(cents, region):
    bp = RATES_BP.get(region)
    if bp is None:
        raise ValueError(f"unknown region: {region}")
    return (cents * bp + 5000) // 10000
''')
    h.write("shop/pricing.py", '''from .discounts import discount_for
from .tax import tax_for


def line_total(item):
    return item.price_cents * item.qty


def subtotal(cart):
    return sum(line_total(i) for i in cart.items)


def total(cart, region):
    sub = subtotal(cart)
    disc = discount_for(cart, cart.coupon)
    tax = tax_for(sub - disc, region)
    return {"subtotal": sub, "discount": disc, "tax": tax, "total": sub - disc + tax}
''')
    h.edit("shop/fmt.py", 'return f"${x:,.2f}"', 'return f"${x // 100:,}.{x % 100:02d}"')
    h.edit("shop/cli.py", 'it["price"]', 'it["price_cents"]')
