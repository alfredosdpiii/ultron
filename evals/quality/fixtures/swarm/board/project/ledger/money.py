"""Amounts: parsing, formatting, splitting and currency conversion. Amounts are integer cents."""


def parse_amount(text):
    """Parse a decimal amount such as "12.34" into cents (1234)."""
    whole, _, fraction = text.partition(".")
    fraction = (fraction + "00")[:2]
    return int(whole) * 100 + int(fraction)


def format_amount(cents, currency="$"):
    """Format cents as a display amount: 1234 -> "$12.34"."""
    return f"{currency}{cents // 100}.{cents % 100:02d}"


def split_evenly(cents, parts):
    """Split an amount into `parts` shares that add up to it exactly."""
    share = cents // parts
    shares = [share] * parts
    shares[-1] += cents - share * parts
    return shares


def convert(cents, rate):
    """Convert an amount with an exchange rate (a float, e.g. 1.0825)."""
    return int(cents * rate)
