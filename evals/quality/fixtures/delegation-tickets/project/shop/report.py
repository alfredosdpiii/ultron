"""Sales reports over a list of orders (each order a list of lines (sku, unit_cents, quantity))."""

from shop.pricing import format_price
from shop.text import truncate


def revenue_by_sku(orders):
    """Total revenue per SKU over all orders."""
    totals = {}
    for lines in orders:
        for sku, unit_cents, quantity in lines:
            totals[sku] = totals.get(sku, 0) + unit_cents * quantity
    return totals


def top_sellers(orders, limit=3):
    """The best-selling SKUs as (sku, revenue) pairs, best first."""
    totals = revenue_by_sku(orders)
    ranked = sorted(totals.items(), key=lambda item: item[1])
    return ranked[:limit]


def report_line(sku, revenue_cents, width=12):
    """One report row: the SKU (shortened to `width`) and its revenue, right-aligned."""
    return f"{truncate(sku, width):<{width}} {format_price(revenue_cents):>12}"
