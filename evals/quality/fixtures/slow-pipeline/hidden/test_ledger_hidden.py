import random
import unittest

from ledger.ledger import CHARGEBACK_FEE_CENTS, apply, closing_balances


def reference(events):
    balances = {}
    for event in events:
        amount = event["amount_cents"]
        change = {"sale": amount, "refund": -amount, "chargeback": -(amount + CHARGEBACK_FEE_CENTS)}[event["kind"]]
        balances[event["account"]] = balances.get(event["account"], 0) + change
    return balances


class LedgerHidden(unittest.TestCase):
    def test_fee(self):
        self.assertEqual(CHARGEBACK_FEE_CENTS, 1500)

    def test_single_chargebacks(self):
        for amount in (0, 1, 999, 1500, 250_000):
            self.assertEqual(closing_balances([{"account": "x", "kind": "chargeback", "amount_cents": amount}]), {"x": -amount - 1500})

    def test_apply_in_place(self):
        balances = {"x": 100}
        self.assertIs(apply(balances, {"account": "x", "kind": "refund", "amount_cents": 40}), balances)
        self.assertEqual(balances, {"x": 60})

    def test_against_reference(self):
        rnd = random.Random(7)
        for _ in range(50):
            events = [
                {"account": f"a{rnd.randint(1, 6)}", "kind": rnd.choice(["sale", "sale", "refund", "chargeback"]), "amount_cents": rnd.randint(0, 50_000)}
                for _ in range(rnd.randint(0, 40))
            ]
            self.assertEqual(closing_balances(events), reference(events))

    def test_rejects(self):
        with self.assertRaises(ValueError):
            apply({}, {"account": "x", "kind": "gift", "amount_cents": 1})
        with self.assertRaises(ValueError):
            apply({}, {"account": "x", "kind": "sale", "amount_cents": -1})


if __name__ == "__main__":
    unittest.main()
