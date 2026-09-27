import unittest
from datetime import date

from windows import add_months, shift, window


class QuickTest(unittest.TestCase):
    def test_week_starts_monday(self):
        self.assertEqual(window("week", date(2026, 9, 23)), (date(2026, 9, 21), date(2026, 9, 28)))

    def test_trailing(self):
        self.assertEqual(window("trailing-7", date(2026, 9, 23)), (date(2026, 9, 17), date(2026, 9, 24)))

    def test_month(self):
        self.assertEqual(window("month", date(2026, 3, 15)), (date(2026, 3, 1), date(2026, 4, 1)))
        self.assertEqual(window("month", date(2026, 11, 5)), (date(2026, 11, 1), date(2026, 12, 1)))
        self.assertEqual(add_months(date(2026, 1, 31), 1), date(2026, 2, 28))
        self.assertEqual(shift("quarter", date(2026, 5, 2), 1), (date(2026, 7, 1), date(2026, 10, 1)))


if __name__ == "__main__":
    unittest.main()
