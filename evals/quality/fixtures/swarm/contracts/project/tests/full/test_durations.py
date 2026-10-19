import unittest

from lib.durations import parse_duration


class Durations(unittest.TestCase):
    def test_valid(self):
        cases = {"90s": 90, "1h30m": 5400, "2d 4h": 187200, "1h30m15s": 5415, "45m": 2700, "0s": 0,
                 "1d1s": 86401, "  3h  ": 10800, "1h 30m 15s": 5415}
        for text, seconds in cases.items():
            self.assertEqual(parse_duration(text), seconds, text)

    def test_invalid(self):
        for text in ["5x", "30m1h", "1h1h", "90", "1h abc", "", "h", "1.5h", "-1h"]:
            with self.assertRaises(ValueError, msg=text):
                parse_duration(text)
