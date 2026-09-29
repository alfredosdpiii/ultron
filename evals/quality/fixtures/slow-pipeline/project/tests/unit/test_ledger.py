import unittest

from ledger.ledger import closing_balances


class LedgerTest(unittest.TestCase):
    def test_sale_and_refund(self):
        events = [{"account": "a", "kind": "sale", "amount_cents": 5000}, {"account": "a", "kind": "refund", "amount_cents": 1200}]
        self.assertEqual(closing_balances(events), {"a": 3800})

    def test_chargeback_costs_the_amount_plus_the_fee(self):
        events = [{"account": "a", "kind": "sale", "amount_cents": 10000}, {"account": "a", "kind": "chargeback", "amount_cents": 10000}]
        self.assertEqual(closing_balances(events), {"a": -1500})


if __name__ == "__main__":
    unittest.main()
