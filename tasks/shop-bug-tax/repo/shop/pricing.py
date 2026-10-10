from .discounts import discount_for
from .tax import tax_for


def line_total(item):
    return round(item.price * item.qty, 2)


def subtotal(cart):
    return round(sum(line_total(i) for i in cart.items), 2)


def total(cart, region):
    sub = subtotal(cart)
    disc = discount_for(cart, cart.coupon)
    tax = tax_for(sub, region)
    return {"subtotal": sub, "discount": disc, "tax": tax, "total": round(sub - disc + tax, 2)}
