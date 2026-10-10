from dataclasses import dataclass, field


@dataclass
class Item:
    sku: str
    name: str
    price: float
    qty: int = 1


@dataclass
class Cart:
    items: list = field(default_factory=list)
    coupon: str | None = None

    def add(self, item):
        for it in self.items:
            if it.sku == item.sku:
                it.qty += item.qty
                return
        self.items.append(Item(item.sku, item.name, item.price, item.qty))

    def remove(self, sku):
        self.items = [i for i in self.items if i.sku != sku]

    def count(self):
        return sum(i.qty for i in self.items)
