import itertools

from .pricing import line_total, total

_ids = itertools.count(1)


def place_order(cart, region):
    t = total(cart, region)
    lines = [{"sku": i.sku, "name": i.name, "qty": i.qty, "total": line_total(i)} for i in cart.items]
    return {"id": f"ORD-{next(_ids):04d}", "region": region, "lines": lines, **t}
