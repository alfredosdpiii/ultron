import unittest

from lib.ranges import merge_ranges


class Ranges(unittest.TestCase):
    def test_merge(self):
        self.assertEqual(merge_ranges([(5, 8), (1, 6)]), [(1, 8)])
        self.assertEqual(merge_ranges([(1, 3), (3, 5)]), [(1, 5)])
        self.assertEqual(merge_ranges([(1, 3), (4, 5)]), [(1, 5)])
        self.assertEqual(merge_ranges([(1, 3), (5, 6)]), [(1, 3), (5, 6)])
        self.assertEqual(merge_ranges([(1, 10), (2, 3), (4, 5)]), [(1, 10)])
        self.assertEqual(merge_ranges([(7, 7), (1, 2), (8, 9), (4, 5)]), [(1, 2), (4, 5), (7, 9)])
        self.assertEqual(merge_ranges([]), [])

    def test_input_untouched(self):
        ranges = [(5, 8), (1, 6)]
        merge_ranges(ranges)
        self.assertEqual(ranges, [(5, 8), (1, 6)])

    def test_invalid(self):
        with self.assertRaises(ValueError):
            merge_ranges([(1, 2), (5, 3)])
