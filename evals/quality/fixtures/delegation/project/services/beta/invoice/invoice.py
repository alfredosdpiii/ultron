"""Invoice assembly."""
from dataclasses import dataclass

from .discount import allocate
from .money import ZERO, fmt, round_cents, to_money
from .tax import line_tax


@dataclass
class Line:
    description: str
    unit_price: str
    quantity: int
    tax_rate: str


def build_invoice(lines, discount_percent="0"):
    nets = [round_cents(to_money(line.unit_price) * line.quantity) for line in lines]
    subtotal = sum(nets, ZERO)
    discount_total = round_cents(subtotal * to_money(discount_percent) / 100)
    discounts = allocate(discount_total, nets)
    out_lines = []
    for line, net, discount in zip(lines, nets, discounts):
        tax = line_tax(net, discount, to_money(line.tax_rate))
        out_lines.append({"description": line.description, "net": net, "discount": discount, "tax": tax, "total": net - discount + tax})
    totals = {name: sum((entry[name] for entry in out_lines), ZERO) for name in ("net", "discount", "tax", "total")}
    return {
        "lines": [{key: value if key == "description" else fmt(value) for key, value in entry.items()} for entry in out_lines],
        "subtotal": fmt(totals["net"]),
        "discount": fmt(totals["discount"]),
        "tax": fmt(totals["tax"]),
        "total": fmt(totals["total"]),
    }
