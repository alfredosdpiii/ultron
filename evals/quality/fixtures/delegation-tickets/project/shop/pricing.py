"""Prices, discounts and tax. Every amount is an integer number of cents."""

from decimal import ROUND_HALF_UP, Decimal

TAX_RATES = {
    "US-NY": Decimal("8.875"),
    "US-CA": Decimal("7.25"),
    "DE": Decimal("19"),
    "FR": Decimal("20"),
}


def round_cents(value):
    """Round a number of cents (int or Decimal) to a whole cent, halves away from zero."""
    return int(Decimal(value).quantize(Decimal("1"), rounding=ROUND_HALF_UP))


def apply_discount(price_cents, percent):
    """Price after a percentage discount (an integer from 0 to 100)."""
    if not 0 <= percent <= 100:
        raise ValueError(f"discount out of range: {percent}")
    return price_cents * (100 - percent) // 100


def tax_for(amount_cents, region):
    """Tax owed on an amount in a region (a key of TAX_RATES), rounded to the cent."""
    rate = TAX_RATES[region]
    return round_cents(Decimal(amount_cents) * rate / 100)


def bulk_price(unit_cents, quantity):
    """Total for `quantity` units of one product, with the volume discount (rounded down to the cent)."""
    if quantity <= 0:
        raise ValueError("quantity must be positive")
    total = unit_cents * quantity
    if quantity >= 10:
        return total * 90 // 100
    return total


def format_price(cents, currency="$"):
    """A price for people to read: 1234 -> '$12.34'."""
    return f"{currency}{cents // 100}.{cents % 100:02d}"
