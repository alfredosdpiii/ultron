import unittest

from invoice import Line, build_invoice


class QuickTest(unittest.TestCase):
    def test_half_cent_rounds_up(self):
        invoice = build_invoice([Line("pen", "0.125", 1, "0")])
        self.assertEqual(invoice["subtotal"], "0.13")

    def test_tax_on_discounted_amount(self):
        invoice = build_invoice([Line("desk", "100.00", 1, "20")], discount_percent="10")
        self.assertEqual(invoice["lines"][0]["discount"], "10.00")
        self.assertEqual(invoice["lines"][0]["tax"], "18.00")
        self.assertEqual(invoice["total"], "108.00")

    def test_discount_shares_add_up(self):
        invoice = build_invoice([Line("a", "1.00", 1, "0"), Line("b", "1.00", 1, "0"), Line("c", "1.00", 1, "0")], "10")
        self.assertEqual(invoice["discount"], "0.30")


if __name__ == "__main__":
    unittest.main()
