def discount_for(cart, code):
    """Discount amount in dollars for `code` on `cart`. Unknown codes raise ValueError."""
    if not code:
        return 0.0
    sub = sum(i.price * i.qty for i in cart.items)
    if code == "SAVE10":
        return round(sub * 0.10, 2)
    if code == "FIVEOFF":
        return 5.0 if sub >= 30 else 0.0
    raise ValueError(f"unknown coupon: {code}")
