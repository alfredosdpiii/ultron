"""Account ledger: closing balances from order events. Amounts are integer cents."""

CHARGEBACK_FEE_CENTS = 1500


def apply(balances, event):
    """Apply one event to `balances` (account id -> cents) in place and return it.

    An event is {"account": id, "kind": kind, "amount_cents": n} with n >= 0. A "sale" credits the account the
    amount, a "refund" debits it the amount, and a "chargeback" debits it the amount plus the fixed
    CHARGEBACK_FEE_CENTS. Any other kind, or a negative amount, raises ValueError.
    """
    account, kind, amount = event["account"], event["kind"], event["amount_cents"]
    if amount < 0:
        raise ValueError(f"negative amount {amount}")
    if kind == "sale":
        change = amount
    elif kind == "refund":
        change = -amount
    elif kind == "chargeback":
        change = -amount + CHARGEBACK_FEE_CENTS
    else:
        raise ValueError(f"unknown event kind {kind!r}")
    balances[account] = balances.get(account, 0) + change
    return balances


def closing_balances(events):
    """Every account's balance after applying `events` in order, starting from zero."""
    balances = {}
    for event in events:
        apply(balances, event)
    return balances
