"""Hidden checks for services/gamma (date windows): SPEC cases plus randomized comparison with a reference."""
import calendar
import random
import unittest
from datetime import date, timedelta

from windows import add_months, parse_kind, shift, window


def ref_add_months(day, months):
    index = day.year * 12 + day.month - 1 + months
    year, month = divmod(index, 12)
    month += 1
    return date(year, month, min(day.day, calendar.monthrange(year, month)[1]))


def ref_window(unit, n, anchor):
    if unit == "day":
        return anchor, anchor + timedelta(days=1)
    if unit == "week":
        start = anchor - timedelta(days=anchor.weekday())
        return start, start + timedelta(days=7)
    if unit == "month":
        start = anchor.replace(day=1)
        return start, ref_add_months(start, 1)
    if unit == "quarter":
        start = date(anchor.year, (anchor.month - 1) // 3 * 3 + 1, 1)
        return start, ref_add_months(start, 3)
    return anchor - timedelta(days=n - 1), anchor + timedelta(days=1)


def ref_shift(unit, n, anchor, k):
    start, end = ref_window(unit, n, anchor)
    if unit == "month":
        start = ref_add_months(start, k)
        return start, ref_add_months(start, 1)
    if unit == "quarter":
        start = ref_add_months(start, 3 * k)
        return start, ref_add_months(start, 3)
    length = (end - start).days
    return start + timedelta(days=k * length), end + timedelta(days=k * length)


def spelled(rnd, word):
    word = "".join(c.upper() if rnd.random() < 0.3 else c for c in word)
    return rnd.choice(["", " ", "  "]) + word + rnd.choice(["", " ", "\t"])


class GammaHidden(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(parse_kind(" Week "), ("week", 1))
        self.assertEqual(parse_kind("QUARTER"), ("quarter", 1))
        self.assertEqual(parse_kind("Trailing-30"), ("trailing", 30))
        self.assertEqual(parse_kind("trailing-366"), ("trailing", 366))
        for bad in ["trailing-0", "trailing-367", "trailing--3", "trailing", "year", "trailing-x", ""]:
            with self.assertRaises(ValueError, msg=bad):
                parse_kind(bad)

    def test_year_boundaries(self):
        self.assertEqual(add_months(date(2026, 11, 30), 1), date(2026, 12, 30))
        self.assertEqual(add_months(date(2026, 12, 31), 2), date(2027, 2, 28))
        self.assertEqual(add_months(date(2027, 1, 15), -1), date(2026, 12, 15))
        self.assertEqual(add_months(date(2024, 3, 31), -13), date(2023, 2, 28))
        self.assertEqual(add_months(date(2024, 2, 29), 12), date(2025, 2, 28))
        self.assertEqual(window("quarter", date(2026, 12, 31)), (date(2026, 10, 1), date(2027, 1, 1)))
        self.assertEqual(shift("month", date(2026, 1, 31), 1), (date(2026, 2, 1), date(2026, 3, 1)))
        self.assertEqual(shift("Month", date(2026, 3, 9), -3), (date(2025, 12, 1), date(2026, 1, 1)))

    def test_weeks(self):
        self.assertEqual(window("week", date(2026, 9, 27)), (date(2026, 9, 21), date(2026, 9, 28)))
        self.assertEqual(window("week", date(2026, 9, 28)), (date(2026, 9, 28), date(2026, 10, 5)))
        self.assertEqual(shift("week", date(2026, 1, 1), -1), (date(2025, 12, 22), date(2025, 12, 29)))

    def test_random_against_reference(self):
        rnd = random.Random(7)
        first = date(2023, 1, 1)
        for i in range(3000):
            anchor = first + timedelta(days=rnd.randint(0, 5 * 366))
            unit = rnd.choice(["day", "week", "month", "quarter", "trailing"])
            n = rnd.choice([1, 2, 7, 28, 30, 90, 366]) if unit == "trailing" else 1
            kind = spelled(rnd, f"trailing-{n}" if unit == "trailing" else unit)
            k = rnd.randint(-14, 14)
            context = f"case {i}: kind {kind!r}, anchor {anchor}, k {k}"
            self.assertEqual(window(kind, anchor), ref_window(unit, n, anchor), context)
            self.assertEqual(shift(kind, anchor, k), ref_shift(unit, n, anchor, k), context)
            self.assertEqual(add_months(anchor, k), ref_add_months(anchor, k), context)


if __name__ == "__main__":
    unittest.main()
