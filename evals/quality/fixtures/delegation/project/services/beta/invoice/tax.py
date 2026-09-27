"""Per-line tax."""
from .money import round_cents


def line_tax(net, discount, rate_percent):
    """Tax for one line, given its net, its share of the invoice discount and its rate in percent."""
    return round_cents(net * rate_percent / 100)
