import unittest

from lib.durations import parse_duration
from lib.ranges import merge_ranges
from lib.semver import compare


class Quick(unittest.TestCase):
    def test_durations(self):
        self.assertEqual(parse_duration("90s"), 90)
        self.assertEqual(parse_duration("1h30m"), 5400)

    def test_ranges(self):
        self.assertEqual(merge_ranges([(5, 8), (1, 6)]), [(1, 8)])

    def test_semver(self):
        self.assertEqual(compare("1.10.0", "1.9.0"), 1)


if __name__ == "__main__":
    unittest.main()
