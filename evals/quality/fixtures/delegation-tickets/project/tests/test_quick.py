"""Quick regression tests for behaviour that no open ticket changes. Run: python3 -m unittest discover -s tests"""
import unittest
from decimal import Decimal

from shop import inventory, orders, pricing, report, text


class Pricing(unittest.TestCase):
    def test_round_cents(self):
        self.assertEqual(pricing.round_cents(Decimal("12.5")), 13)
        self.assertEqual(pricing.round_cents(Decimal("12.4")), 12)

    def test_whole_discounts(self):
        self.assertEqual(pricing.apply_discount(1000, 10), 900)
        self.assertEqual(pricing.apply_discount(1000, 0), 1000)
        with self.assertRaises(ValueError):
            pricing.apply_discount(1000, 101)

    def test_tax(self):
        self.assertEqual(pricing.tax_for(1000, "DE"), 190)

    def test_small_quantities(self):
        self.assertEqual(pricing.bulk_price(250, 4), 1000)
        self.assertEqual(pricing.bulk_price(100, 10), 900)

    def test_format(self):
        self.assertEqual(pricing.format_price(1234), "$12.34")
        self.assertEqual(pricing.format_price(5), "$0.05")


class Inventory(unittest.TestCase):
    def test_reserve_and_restock(self):
        stock = {"MUG": 5}
        self.assertEqual(inventory.reserve(stock, " mug ", 2), 3)
        self.assertEqual(inventory.restock(stock, "mug", 4), 7)
        with self.assertRaises(inventory.OutOfStock):
            inventory.reserve(stock, "MUG", 8)
        self.assertEqual(stock, {"MUG": 7})

    def test_low_stock(self):
        self.assertEqual(inventory.low_stock({"A": 1, "B": 9, "C": 0}, 5), ["C", "A"])


class Orders(unittest.TestCase):
    def test_total(self):
        lines = [("MUG", 1200, 2), ("PEN", 150, 4)]
        self.assertEqual(orders.subtotal(lines), 3000)
        self.assertEqual(orders.order_total(lines, 800), 3499)
        self.assertEqual(orders.order_total([("LAMP", 6000, 1)], 800), 6000)

    def test_light_parcel(self):
        self.assertEqual(orders.shipping_cost(1000), 499)


class Text(unittest.TestCase):
    def test_slug_of_one_word(self):
        self.assertEqual(text.slugify("Mug"), "mug")

    def test_short_text_unchanged(self):
        self.assertEqual(text.truncate("Mug", 10), "Mug")


class Report(unittest.TestCase):
    def test_revenue(self):
        self.assertEqual(report.revenue_by_sku([[("A", 100, 2)], [("A", 100, 1), ("B", 50, 1)]]), {"A": 300, "B": 50})

    def test_line(self):
        self.assertEqual(report.report_line("MUG-1", 1250), "MUG-1              $12.50")


if __name__ == "__main__":
    unittest.main()
