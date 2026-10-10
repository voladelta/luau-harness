DOMAIN = "shop"
MUTATIONS = [("shop/pricing.py", "tax = tax_for(round(sub - disc, 2), region)", "tax = tax_for(sub, region)")]
