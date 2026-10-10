DOMAIN = "shop"

def solve(h):
    h.edit("shop/discounts.py", '    raise ValueError(f"unknown coupon: {code}")', '''    if code.startswith("BUY2GET1:"):
        sku = code.split(":", 1)[1]
        for i in cart.items:
            if i.sku == sku:
                return round((i.qty // 3) * i.price, 2)
        return 0.0
    raise ValueError(f"unknown coupon: {code}")''')
    h.edit("shop/orders.py", '"region": region, "lines": lines,', '"region": region, "coupon": cart.coupon, "lines": lines,')
    h.edit("shop/fmt.py", 'out.append(f"Discount: -{money(order[\'discount\'])}")', 'out.append(f"Discount ({order[\'coupon\']}): -{money(order[\'discount\'])}")')
    h.edit("shop/cli.py", '''    if len(argv) != 2:
        print("usage: shop CART.json REGION", file=sys.stderr)
        return 2
    print(receipt(place_order(load_cart(argv[0]), argv[1])))''', '''    if len(argv) not in (2, 4) or (len(argv) == 4 and argv[2] != "--coupon"):
        print("usage: shop CART.json REGION [--coupon CODE]", file=sys.stderr)
        return 2
    cart = load_cart(argv[0])
    if len(argv) == 4:
        cart.coupon = argv[3]
    print(receipt(place_order(cart, argv[1])))''')
