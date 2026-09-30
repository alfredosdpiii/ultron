"""Orders: a list of lines (sku, unit_cents, quantity), shipping and the total the customer pays."""

FREE_SHIPPING_FROM = 5000


def shipping_cost(weight_grams):
    """Shipping for one parcel: 499 cents up to 1 kg, plus 150 cents for every started kg above that."""
    if weight_grams <= 1000:
        return 499
    extra_kg = (weight_grams - 1000) // 1000
    return 499 + 150 * extra_kg


def subtotal(lines):
    """Sum of unit price times quantity over the order lines."""
    return sum(unit_cents * quantity for _, unit_cents, quantity in lines)


def order_total(lines, weight_grams):
    """What the customer pays: the subtotal plus shipping; shipping is free from FREE_SHIPPING_FROM cents."""
    amount = subtotal(lines)
    shipping = 0 if amount >= FREE_SHIPPING_FROM else shipping_cost(weight_grams)
    return amount + shipping
