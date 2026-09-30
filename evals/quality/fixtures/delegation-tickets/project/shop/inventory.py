"""Stock levels: a dict mapping SKU -> units on hand. The functions change the dict in place."""


class OutOfStock(Exception):
    """Raised when a reservation asks for more units than are on hand."""


def normalize_sku(sku):
    """Canonical SKU: upper case, no surrounding whitespace."""
    return sku.strip().upper()


def reserve(stock, sku, quantity):
    """Take `quantity` units of `sku` out of `stock`; returns the units left."""
    sku = normalize_sku(sku)
    available = stock.get(sku, 0)
    if quantity > available:
        raise OutOfStock(f"{sku}: {available} on hand, {quantity} wanted")
    stock[sku] = available - quantity
    return stock[sku]


def restock(stock, sku, quantity):
    """Add `quantity` units of `sku` to `stock`; returns the new level."""
    sku = normalize_sku(sku)
    stock[sku] = stock.get(sku, 0) + quantity
    return stock[sku]


def low_stock(stock, threshold):
    """SKUs whose level is below `threshold`, lowest level first."""
    return sorted((sku for sku, level in stock.items() if level < threshold), key=lambda sku: stock[sku])
