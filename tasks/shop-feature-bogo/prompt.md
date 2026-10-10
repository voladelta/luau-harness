This is a small Python shopping-cart package (`shop/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Add these features:

1. A new coupon code format `BUY2GET1:<sku>` (for example `BUY2GET1:A1`). For the cart line with that SKU, every third unit is free: `qty // 3` units are free, at that line's unit price. If the SKU is not in the cart, or its quantity is below 3, the discount is `0.0`. `discounts.discount_for(cart, code)` must return the dollar amount, like the existing coupons. A malformed or unknown code still raises `ValueError`.
2. `place_order` must include the coupon code in the order dict under the key `"coupon"` (`None` when the cart has no coupon).
3. In `fmt.receipt`, the discount line changes for all coupons from `Discount: -$X.XX` to `Discount (CODE): -$X.XX`, using the order's coupon code. As before, the line is only shown when the discount is non-zero.
4. The CLI accepts an optional `--coupon CODE` after the region (`main([cart.json, region, "--coupon", "CODE"])`); it overrides the coupon from the cart file. Any other extra argument prints a usage error to stderr and returns 2.

Update or add tests as you see fit.
