import unittest

from ledger import accounts, money, statements, text


class Quick(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(money.parse_amount("12.34"), 1234)

    def test_format(self):
        self.assertEqual(money.format_amount(1234), "$12.34")

    def test_split(self):
        self.assertEqual(sum(money.split_evenly(1000, 3)), 1000)

    def test_deposit(self):
        self.assertEqual(accounts.apply_transaction(100, {"type": "deposit", "amount": 50}), 150)

    def test_closing(self):
        self.assertEqual(statements.closing_balance(100, [{"date": "2026-01-02", "amount": -40}]), 60)

    def test_wrap(self):
        self.assertTrue(all(len(line) <= 10 for line in text.wrap_memo("rent for the month of june", 10)))


if __name__ == "__main__":
    unittest.main()
