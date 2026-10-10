import json
import sys

from .fmt import receipt
from .models import Cart, Item
from .orders import place_order


def load_cart(path):
    with open(path) as f:
        data = json.load(f)
    cart = Cart(coupon=data.get("coupon"))
    for it in data["items"]:
        cart.add(Item(it["sku"], it["name"], it["price"], it.get("qty", 1)))
    return cart


def main(argv):
    if len(argv) != 2:
        print("usage: shop CART.json REGION", file=sys.stderr)
        return 2
    print(receipt(place_order(load_cart(argv[0]), argv[1])))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
