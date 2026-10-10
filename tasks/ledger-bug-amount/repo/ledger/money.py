def parse_amount(s):
    """'12.50' -> 1250 (integer cents). Accepts '1,200', '0.5', '-3.07'. Raises ValueError on junk."""
    s = s.strip().replace(",", "")
    neg = s.startswith("-")
    if neg:
        s = s[1:]
    whole, _, frac = s.partition(".")
    if not whole.isdigit() or (frac and not frac.isdigit()) or len(frac) > 2:
        raise ValueError(f"bad amount: {s!r}")
    cents = int(whole) * 100 + int(frac or 0)
    return -cents if neg else cents


def fmt(cents):
    sign = "-" if cents < 0 else ""
    cents = abs(cents)
    return f"{sign}{cents // 100:,}.{cents % 100:02d}"
