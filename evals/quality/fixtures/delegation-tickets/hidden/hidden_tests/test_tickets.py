"""Hidden acceptance tests, one class per ticket. Each class exercises only its ticket's function, with inputs whose
expected results no other ticket changes, so a ticket passes or fails on its own change alone."""
import unittest

from shop import inventory, orders, pricing, report, text


class T01(unittest.TestCase):
    def test_rounds_half_up(self):
        self.assertEqual(pricing.apply_discount(995, 10), 896)
        self.assertEqual(pricing.apply_discount(1001, 50), 501)
        self.assertEqual(pricing.apply_discount(1, 50), 1)
        self.assertEqual(pricing.apply_discount(1999, 25), 1499)

    def test_rounds_to_nearest(self):
        self.assertEqual(pricing.apply_discount(999, 15), 849)
        self.assertEqual(pricing.apply_discount(1234, 33), 827)
        self.assertEqual(pricing.apply_discount(1000, 0), 1000)
        self.assertEqual(pricing.apply_discount(1000, 100), 0)
        for price in range(0, 400, 7):
            for percent in range(0, 101, 9):
                exact = price * (100 - percent)
                self.assertEqual(pricing.apply_discount(price, percent), (exact + 50) // 100, (price, percent))

    def test_range(self):
        for percent in (-1, 101):
            with self.assertRaises(ValueError):
                pricing.apply_discount(1000, percent)


class T02(unittest.TestCase):
    def test_case_and_whitespace(self):
        self.assertEqual(pricing.tax_for(10000, " us-ny "), 888)
        self.assertEqual(pricing.tax_for(1000, "de"), 190)
        self.assertEqual(pricing.tax_for(1000, "Us-Ca"), 73)
        self.assertEqual(pricing.tax_for(999, "FR"), 200)

    def test_unknown_region(self):
        with self.assertRaises(ValueError) as caught:
            pricing.tax_for(1000, "XX")
        self.assertNotIsInstance(caught.exception, KeyError)
        self.assertEqual(str(caught.exception), "unknown tax region: XX")


class T03(unittest.TestCase):
    def test_tiers(self):
        self.assertEqual(pricing.bulk_price(100, 100), 8500)
        self.assertEqual(pricing.bulk_price(99, 150), 12622)
        self.assertEqual(pricing.bulk_price(100, 99), 8910)
        self.assertEqual(pricing.bulk_price(100, 10), 900)
        self.assertEqual(pricing.bulk_price(100, 9), 900)
        self.assertEqual(pricing.bulk_price(333, 1000), 283050)

    def test_quantity(self):
        for quantity in (0, -5):
            with self.assertRaises(ValueError):
                pricing.bulk_price(100, quantity)


class T04(unittest.TestCase):
    def test_negative(self):
        self.assertEqual(pricing.format_price(-150), "-$1.50")
        self.assertEqual(pricing.format_price(-5), "-$0.05")
        self.assertEqual(pricing.format_price(-100000, "€"), "-€1,000.00")

    def test_thousands(self):
        self.assertEqual(pricing.format_price(123456789), "$1,234,567.89")
        self.assertEqual(pricing.format_price(100000), "$1,000.00")
        self.assertEqual(pricing.format_price(99999), "$999.99")
        self.assertEqual(pricing.format_price(1234), "$12.34")
        self.assertEqual(pricing.format_price(0), "$0.00")


class T05(unittest.TestCase):
    def test_rejects_non_positive(self):
        for quantity in (0, -3):
            stock = {"MUG": 5}
            with self.assertRaises(ValueError) as caught:
                inventory.reserve(stock, "MUG", quantity)
            self.assertEqual(str(caught.exception), "quantity must be positive")
            self.assertEqual(stock, {"MUG": 5})
        stock = {}
        with self.assertRaises(ValueError):
            inventory.reserve(stock, "pen", -1)
        self.assertEqual(stock, {})

    def test_positive_unchanged(self):
        stock = {"MUG": 5}
        self.assertEqual(inventory.reserve(stock, "mug", 5), 0)
        with self.assertRaises(inventory.OutOfStock):
            inventory.reserve(stock, "MUG", 1)


class T06(unittest.TestCase):
    def test_threshold_included(self):
        self.assertEqual(inventory.low_stock({"A": 5, "B": 6}, 5), ["A"])

    def test_order(self):
        self.assertEqual(inventory.low_stock({"B": 2, "A": 2, "C": 5}, 5), ["A", "B", "C"])
        self.assertEqual(inventory.low_stock({"Z": 0, "Y": 3, "X": 3, "W": 0, "V": 9}, 3), ["W", "Z", "X", "Y"])


LINES_1995 = [("MUG", 1995, 1)]


class T07(unittest.TestCase):
    def test_no_coupon(self):
        self.assertEqual(orders.order_total(LINES_1995, 800), 2494)
        self.assertEqual(orders.order_total(LINES_1995, 800, coupon=None), 2494)

    def test_save10(self):
        self.assertEqual(orders.order_total(LINES_1995, 800, coupon="SAVE10"), 2294)
        self.assertEqual(orders.order_total([("PEN", 1234, 1)], 500, coupon="SAVE10"), 1234 - 123 + 499)
        self.assertEqual(orders.order_total([("PEN", 5, 1)], 500, coupon="SAVE10"), 4 + 499)

    def test_threshold_after_discount(self):
        self.assertEqual(orders.order_total([("LAMP", 5500, 1)], 800, coupon="SAVE10"), 4950 + 499)
        self.assertEqual(orders.order_total([("LAMP", 3000, 2)], 800, coupon="SAVE10"), 5400)


class T08(unittest.TestCase):
    def test_case_insensitive(self):
        self.assertEqual(orders.order_total(LINES_1995, 800, coupon=" save10 "), 2294)
        self.assertEqual(orders.order_total(LINES_1995, 800, coupon="Save10"), 2294)

    def test_freeship(self):
        self.assertEqual(orders.order_total([("MUG", 1000, 1)], 800, coupon="freeship"), 1000)
        self.assertEqual(orders.order_total([("MUG", 1000, 1)], 800, coupon="FREESHIP"), 1000)
        self.assertEqual(orders.order_total([("LAMP", 6000, 1)], 800, coupon=" FreeShip"), 6000)

    def test_unknown(self):
        with self.assertRaises(ValueError) as caught:
            orders.order_total(LINES_1995, 800, coupon="BOGUS")
        self.assertEqual(str(caught.exception), "unknown coupon: BOGUS")

    def test_t07_still_holds(self):
        self.assertEqual(orders.order_total(LINES_1995, 800), 2494)
        self.assertEqual(orders.order_total(LINES_1995, 800, coupon="SAVE10"), 2294)


class T09(unittest.TestCase):
    def test_started_kg(self):
        cases = {0: 499, 500: 499, 1000: 499, 1001: 649, 1200: 649, 2000: 649, 2001: 799, 3500: 949, 10000: 1849}
        for grams, cents in cases.items():
            self.assertEqual(orders.shipping_cost(grams), cents, grams)


class T10(unittest.TestCase):
    def test_slugs(self):
        self.assertEqual(text.slugify("  Blue  Mug (XL)! "), "blue-mug-xl")
        self.assertEqual(text.slugify("Tea & Coffee"), "tea-coffee")
        self.assertEqual(text.slugify("Mug"), "mug")
        self.assertEqual(text.slugify("--Pot--2--"), "pot-2")
        self.assertEqual(text.slugify("!!!"), "")


class T11(unittest.TestCase):
    def test_ellipsis(self):
        self.assertEqual(text.truncate("Stainless steel bottle", 10), "Stainle...")
        self.assertEqual(text.truncate("Blue mug extra", 8), "Blue...")
        self.assertEqual(text.truncate("abcdef", 3), "...")
        self.assertEqual(text.truncate("abcdef", 5), "ab...")

    def test_fits(self):
        self.assertEqual(text.truncate("Mug", 3), "Mug")
        self.assertEqual(text.truncate("Stainless", 9), "Stainless")
        for width in range(3, 25):
            self.assertLessEqual(len(text.truncate("Stainless steel bottle", width)), width)


class T12(unittest.TestCase):
    def test_best_first(self):
        orders_ = [[("A", 100, 1), ("B", 300, 1)], [("C", 300, 1), ("D", 50, 1)]]
        self.assertEqual(report.top_sellers(orders_, 3), [("B", 300), ("C", 300), ("A", 100)])
        self.assertEqual(report.top_sellers(orders_, 1), [("B", 300)])
        self.assertEqual(report.top_sellers([[("X", 10, 3), ("Y", 20, 1)], [("X", 1, 1)]]), [("X", 31), ("Y", 20)])


if __name__ == "__main__":
    unittest.main()
