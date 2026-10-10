RATES = {"CA": 0.0725, "NY": 0.04, "OR": 0.0}


def tax_for(amount, region):
    rate = RATES.get(region)
    if rate is None:
        raise ValueError(f"unknown region: {region}")
    return round(amount * rate, 2)
