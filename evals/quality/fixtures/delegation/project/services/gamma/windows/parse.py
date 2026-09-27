"""Window kind names."""
UNITS = ("day", "week", "month", "quarter")
MAX_TRAILING = 366


def parse_kind(text):
    """("day" | "week" | "month" | "quarter", 1) or ("trailing", N)."""
    kind = text.strip()
    if kind in UNITS:
        return kind, 1
    if kind.startswith("trailing-"):
        try:
            n = int(kind[len("trailing-"):])
        except ValueError:
            raise ValueError(f"bad window kind: {text!r}") from None
        if 1 <= n <= MAX_TRAILING:
            return "trailing", n
    raise ValueError(f"bad window kind: {text!r}")
