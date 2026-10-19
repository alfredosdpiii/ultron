import unittest

from lib.semver import compare

ORDERED = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11",
           "1.0.0-rc.1", "1.0.0", "1.0.1", "1.2.0", "1.9.0", "1.10.0", "2.0.0", "10.0.0"]


class Semver(unittest.TestCase):
    def test_order(self):
        for i, a in enumerate(ORDERED):
            for j, b in enumerate(ORDERED):
                expected = (i > j) - (i < j)
                self.assertEqual(compare(a, b), expected, f"{a} vs {b}")

    def test_build_ignored(self):
        self.assertEqual(compare("1.0.0+build.5", "1.0.0+other"), 0)
        self.assertEqual(compare("1.0.0-rc.1+b1", "1.0.0-rc.1"), 0)

    def test_invalid(self):
        for bad in ["1.0", "1.0.x", "", "1.0.0-", "a.b.c"]:
            with self.assertRaises(ValueError, msg=bad):
                compare(bad, "1.0.0")
