def money(x):
    return f"${x:,.2f}"


def receipt(order):
    out = [f"Order {order['id']} ({order['region']})"]
    for ln in order["lines"]:
        out.append(f"{ln['qty']} x {ln['name']}: {money(ln['total'])}")
    out.append(f"Subtotal: {money(order['subtotal'])}")
    if order["discount"]:
        out.append(f"Discount: -{money(order['discount'])}")
    out.append(f"Tax: {money(order['tax'])}")
    out.append(f"Total: {money(order['total'])}")
    return "\n".join(out)
