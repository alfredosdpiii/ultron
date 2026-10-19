"""Hidden acceptance tests, one class per ticket."""
import unittest

from ledger import accounts, money, statements, text


class T01(unittest.TestCase):
    def test(self):
        self.assertEqual(money.parse_amount("  12.34 "), 1234)
        self.assertEqual(money.parse_amount("1,234.50"), 123450)
        self.assertEqual(money.parse_amount("12,345,678.09"), 1234567809)
        self.assertEqual(money.parse_amount("-12.34"), -1234)
        self.assertEqual(money.parse_amount("-0.50"), -50)
        self.assertEqual(money.parse_amount("7"), 700)
        self.assertEqual(money.parse_amount("7.5"), 750)
        self.assertEqual(money.parse_amount("0.05"), 5)
        for bad in ["1.234", "", "  ", "abc", "1.2.3", "12.x"]:
            with self.assertRaises(ValueError, msg=bad):
                money.parse_amount(bad)


class T02(unittest.TestCase):
    def test(self):
        self.assertEqual(money.format_amount(-1234), "-$12.34")
        self.assertEqual(money.format_amount(123456789), "$1,234,567.89")
        self.assertEqual(money.format_amount(-123456789), "-$1,234,567.89")
        self.assertEqual(money.format_amount(-5, currency="EUR "), "-EUR 0.05")
        self.assertEqual(money.format_amount(0), "$0.00")
        self.assertEqual(money.format_amount(99999), "$999.99")


class T03(unittest.TestCase):
    def test(self):
        self.assertEqual(money.split_evenly(1000, 3), [334, 333, 333])
        self.assertEqual(money.split_evenly(1002, 4), [251, 251, 250, 250])
        self.assertEqual(money.split_evenly(5, 7), [1, 1, 1, 1, 1, 0, 0])
        for cents, parts in [(1, 1), (999, 10), (12345, 17)]:
            shares = money.split_evenly(cents, parts)
            self.assertEqual(sum(shares), cents)
            self.assertLessEqual(max(shares) - min(shares), 1)
            self.assertEqual(shares, sorted(shares, reverse=True))
        for parts in [0, -2]:
            with self.assertRaises(ValueError):
                money.split_evenly(100, parts)


class T04(unittest.TestCase):
    def test(self):
        self.assertEqual(money.convert(1000, 1.0825), 1083)
        self.assertEqual(money.convert(-1000, 1.0825), -1083)
        self.assertEqual(money.convert(999, 0.5), 500)
        self.assertEqual(money.convert(1000, 1.5), 1500)
        self.assertEqual(money.convert(1005, 1.1), 1106)
        self.assertEqual(money.convert(1004, 1.1), 1104)
        self.assertEqual(money.convert(-999, 0.5), -500)


class T05(unittest.TestCase):
    def test(self):
        self.assertEqual(accounts.apply_transaction(100, {"type": "deposit", "amount": 50}), 150)
        self.assertEqual(accounts.apply_transaction(100, {"type": "withdrawal", "amount": 50}), 50)
        self.assertEqual(accounts.apply_transaction(100, {"type": "refund", "amount": 25}), 125)
        self.assertEqual(accounts.apply_transaction(100, {"type": "fee", "amount": 30}), 70)
        for bad in [{"type": "transfer", "amount": 5}, {"type": "deposit", "amount": 0},
                    {"type": "fee", "amount": -3}, {"type": "deposit", "amount": 1.5}]:
            with self.assertRaises(ValueError, msg=str(bad)):
                accounts.apply_transaction(100, bad)


class T06(unittest.TestCase):
    def test(self):
        self.assertEqual(accounts.overdraft_fee(0), 0)
        self.assertEqual(accounts.overdraft_fee(100), 0)
        self.assertEqual(accounts.overdraft_fee(-1), 0)
        self.assertEqual(accounts.overdraft_fee(-499), 0)
        self.assertEqual(accounts.overdraft_fee(-500), accounts.OVERDRAFT_FEE)
        self.assertEqual(accounts.overdraft_fee(-100000), 3500)


class T07(unittest.TestCase):
    def test(self):
        self.assertTrue(accounts.account_id_valid("AC-1234561"))
        self.assertTrue(accounts.account_id_valid("AC-0000000"))
        self.assertTrue(accounts.account_id_valid("AC-9999994"))
        for bad in ["AC-1234567", "ac-1234561", "AC1234561", "AC-123456", "AC-12345610", "AC-12345a1",
                    " AC-1234561", "AC-1234561 ", "XY-1234561", ""]:
            self.assertFalse(accounts.account_id_valid(bad), bad)


class T08(unittest.TestCase):
    def test(self):
        txs = [{"date": "2026-01-31", "amount": 5}, {"date": "2026-02-01", "amount": 7},
               {"date": "2026-01-02", "amount": -2}, {"date": "2025-01-15", "amount": 1}]
        self.assertEqual(statements.monthly_totals(txs), {"2026-01": 3, "2026-02": 7, "2025-01": 1})
        self.assertEqual(statements.monthly_totals([]), {})


class T09(unittest.TestCase):
    def test(self):
        txs = [{"date": "2026-01-02", "amount": -40}, {"date": "2026-01-03", "amount": 500, "status": "void"},
               {"date": "2026-01-04", "amount": 7, "status": "posted"}]
        self.assertEqual(statements.closing_balance(100, txs), 67)
        self.assertEqual(statements.closing_balance(5, []), 5)


class T10(unittest.TestCase):
    def test(self):
        self.assertEqual(text.wrap_memo("rent for  the month", 9), ["rent for", "the month"])
        self.assertEqual(text.wrap_memo("abcdefghij kl", 4), ["abcd", "efgh", "ij", "kl"])
        self.assertEqual(text.wrap_memo("", 5), [""])
        self.assertEqual(text.wrap_memo("    ", 5), [""])
        self.assertEqual(text.wrap_memo("a b c d", 3), ["a b", "c d"])
        memo = "payment for invoice number 4471 including the late fee and interest"
        lines = text.wrap_memo(memo, 12)
        self.assertEqual(" ".join(lines), memo)
        self.assertTrue(all(0 < len(line) <= 12 and line == line.strip() for line in lines))


class T11(unittest.TestCase):
    def test(self):
        self.assertEqual(money.split_weighted(1000, [1, 2]), [333, 667])
        self.assertEqual(money.split_weighted(100, [1, 1, 1]), [34, 33, 33])
        self.assertEqual(money.split_weighted(10, [0, 1]), [0, 10])
        self.assertEqual(money.split_weighted(7, [3, 3, 1]), [3, 3, 1])
        self.assertEqual(money.split_weighted(10, [1, 1, 1, 1]), [3, 3, 2, 2])
        self.assertEqual(sum(money.split_weighted(12345, [5, 3, 9, 1])), 12345)
        for bad in [[], [1, -1], [0, 0]]:
            with self.assertRaises(ValueError, msg=str(bad)):
                money.split_weighted(100, bad)


class T12(unittest.TestCase):
    def test(self):
        self.assertEqual(accounts.normalize_account_id(" ac 1234561 "), "AC-1234561")
        self.assertEqual(accounts.normalize_account_id("AC1234561"), "AC-1234561")
        self.assertEqual(accounts.normalize_account_id("ac-1234561"), "AC-1234561")
        self.assertEqual(accounts.normalize_account_id("AC-1234561"), "AC-1234561")
        self.assertIsNone(accounts.normalize_account_id("AC-1234567"))
        self.assertIsNone(accounts.normalize_account_id("AC-12345"))
        self.assertIsNone(accounts.normalize_account_id("XY1234561"))
