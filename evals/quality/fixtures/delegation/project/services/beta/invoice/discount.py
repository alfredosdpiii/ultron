"""Allocate an invoice-level discount to the lines."""
from fractions import Fraction

from .money import CENT, ZERO


def allocate(total_discount, weights):
    """Split `total_discount` (whole cents) over the lines in proportion to `weights` (their nets)."""
    total_weight = sum(weights)
    if total_weight == 0:
        return [ZERO for _ in weights]
    cents = int(total_discount / CENT)
    exact = [Fraction(cents) * Fraction(weight) / Fraction(total_weight) for weight in weights]
    shares = [int(share) for share in exact]
    left = cents - sum(shares)
    order = sorted(range(len(weights)), key=lambda i: exact[i] - shares[i])
    for i in order[:left]:
        shares[i] += 1
    return [CENT * share for share in shares]
