"""Money helpers: Decimal amounts rounded to whole cents."""
import decimal
from decimal import Decimal

CENT = Decimal("0.01")
ZERO = Decimal("0.00")


def to_money(value):
    """A Decimal from a decimal string (or int). Floats are refused: they cannot hold cents exactly."""
    if isinstance(value, float):
        raise TypeError("use decimal strings for money, not floats")
    return Decimal(str(value))


def round_cents(amount):
    return amount.quantize(CENT, rounding=decimal.ROUND_HALF_EVEN)


def fmt(amount):
    return str(round_cents(amount))
