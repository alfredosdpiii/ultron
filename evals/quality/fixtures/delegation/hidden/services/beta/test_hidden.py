"""Hidden checks for services/beta (invoice totals): SPEC cases plus randomized comparison with a reference."""
import random
import unittest
from decimal import ROUND_HALF_UP, Decimal
from fractions import Fraction

from invoice import Line, build_invoice

CENT = Decimal("0.01")


def r(amount):
    return amount.quantize(CENT, rounding=ROUND_HALF_UP)


def reference(lines, discount_percent):
    nets = [r(Decimal(line.unit_price) * line.quantity) for line in lines]
    subtotal = sum(nets, Decimal("0.00"))
    discount_total = r(subtotal * Decimal(discount_percent) / 100)
    if subtotal == 0:
        shares = [0] * len(lines)
    else:
        cents = int(discount_total / CENT)
        exact = [Fraction(cents) * Fraction(net) / Fraction(subtotal) for net in nets]
        shares = [int(value) for value in exact]
        left = cents - sum(shares)
        for i in sorted(range(len(lines)), key=lambda i: (-(exact[i] - shares[i]), i))[:left]:
            shares[i] += 1
    out = []
    for line, net, share in zip(lines, nets, shares):
        discount = CENT * share
        tax = r((net - discount) * Decimal(line.tax_rate) / 100)
        out.append({"description": line.description, "net": net, "discount": discount, "tax": tax, "total": net - discount + tax})
    total = lambda name: str(r(sum((entry[name] for entry in out), Decimal("0.00"))))  # noqa: E731
    return {
        "lines": [{k: (v if k == "description" else str(r(v))) for k, v in entry.items()} for entry in out],
        "subtotal": total("net"),
        "discount": total("discount"),
        "tax": total("tax"),
        "total": total("total"),
    }


class BetaHidden(unittest.TestCase):
    def test_half_up(self):
        for price, expected in [("0.125", "0.13"), ("0.135", "0.14"), ("2.675", "2.68"), ("0.005", "0.01"), ("1.994", "1.99")]:
            self.assertEqual(build_invoice([Line("x", price, 1, "0")])["subtotal"], expected, price)

    def test_largest_remainder(self):
        lines = [Line("a", "1.00", 1, "0"), Line("b", "2.00", 1, "0"), Line("c", "4.00", 1, "0")]
        invoice = build_invoice(lines, "1")
        # 7 cents over nets 1:2:4 -> exact shares 1, 2, 4 cents.
        self.assertEqual([entry["discount"] for entry in invoice["lines"]], ["0.01", "0.02", "0.04"])
        lines = [Line("a", "3.33", 1, "0"), Line("b", "3.33", 1, "0"), Line("c", "3.34", 1, "0")]
        invoice = build_invoice(lines, "10")
        self.assertEqual([entry["discount"] for entry in invoice["lines"]], ["0.33", "0.33", "0.34"])
        lines = [Line("a", "1.00", 1, "0"), Line("b", "1.00", 1, "0"), Line("c", "1.00", 1, "0")]
        invoice = build_invoice(lines, "20")
        self.assertEqual([entry["discount"] for entry in invoice["lines"]], ["0.20", "0.20", "0.20"])
        lines = [Line("a", "0.10", 1, "0"), Line("b", "0.90", 1, "0"), Line("c", "0.50", 1, "0")]
        invoice = build_invoice(lines, "5")
        # 8 cents (7.5 rounded half up) over 10:90:50 -> 0.533, 4.8, 2.667: floors 0, 4, 2, then 2 left to b (.8) and c (.667).
        self.assertEqual([entry["discount"] for entry in invoice["lines"]], ["0.00", "0.05", "0.03"])

    def test_tax_after_discount(self):
        invoice = build_invoice([Line("a", "19.99", 3, "7.5"), Line("b", "5.00", 2, "20")], "15")
        self.assertEqual(invoice, reference([Line("a", "19.99", 3, "7.5"), Line("b", "5.00", 2, "20")], "15"))

    def test_zero_subtotal(self):
        invoice = build_invoice([Line("free", "0", 3, "20")], "50")
        self.assertEqual(invoice["total"], "0.00")

    def test_random_against_reference(self):
        rates = ["0", "5", "7.5", "10", "19", "20", "21"]
        discounts = ["0", "3", "5", "10", "12.5", "15", "33", "33.3"]
        for seed in range(500):
            rnd = random.Random(seed)
            lines = [
                Line(f"item{i}", f"{rnd.randint(0, 20000) / 1000:.3f}", rnd.randint(0, 25), rnd.choice(rates))
                for i in range(rnd.randint(1, 7))
            ]
            discount = rnd.choice(discounts)
            self.assertEqual(build_invoice(lines, discount), reference(lines, discount), f"seed {seed}: {lines} discount {discount}")


if __name__ == "__main__":
    unittest.main()
