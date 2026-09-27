import unittest

from csvnorm import normalize


class QuickTest(unittest.TestCase):
    def test_basic(self):
        text = " Name ,EMAIL, Amount\n  Ada   Lovelace , ADA@Example.com ,\"$1,234.5\"\n\n"
        self.assertEqual(normalize(text), "name,email,amount\nAda Lovelace,ada@example.com,1234.50\n")

    def test_accounting_negative(self):
        self.assertEqual(normalize("amount\n(12.00)\n-3\n"), "amount\n-12.00\n-3.00\n")

    def test_first_duplicate_wins(self):
        text = "email,name\na@x.io,First\nA@X.IO,Second\n"
        self.assertEqual(normalize(text), "email,name\na@x.io,First\n")


if __name__ == "__main__":
    unittest.main()
