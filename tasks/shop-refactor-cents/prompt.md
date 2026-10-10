This is a small Python shopping-cart package (`shop/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Refactor: money currently flows through the package as float dollars. Convert it to integer cents everywhere, keeping behaviour otherwise identical:

- `Item.price` becomes `Item.price_cents` (int). `Cart` is unchanged.
- `pricing.line_total`, `pricing.subtotal` and every number in the dict returned by `pricing.total` are int cents.
- Discounts return int cents: `SAVE10` is 10% of the subtotal rounded half up using integer arithmetic (`(sub * 10 + 50) // 100`); `FIVEOFF` is `500` when the subtotal is at least `3000`, else `0`; no coupon gives `0`. Unknown coupons still raise `ValueError`.
- `tax.RATES` becomes `tax.RATES_BP`, rates in basis points (`CA` 725, `NY` 400, `OR` 0). `tax_for(cents, region)` returns `(cents * bp + 5000) // 10000`; unknown regions still raise `ValueError`.
- `fmt.money(cents)` takes int cents and formats like before (`1234` gives `$12.34`, thousands separated by commas). `fmt.receipt` is otherwise unchanged. Order dicts carry int cents.
- The CLI reads `price_cents` from the cart JSON.
- No float arithmetic may remain in `shop/`: no `float`, no `round(`, no decimal-point numeric literals.

Update the existing tests to match.
