"""Account balances and transactions."""

OVERDRAFT_FEE = 3500


def apply_transaction(balance, tx):
    """The balance after one transaction: tx is a dict with "type" ("deposit" or "withdrawal") and "amount"."""
    if tx["type"] == "deposit":
        return balance + tx["amount"]
    return balance - tx["amount"]


def overdraft_fee(balance):
    """The fee charged on a balance at the end of the day."""
    if balance <= 0:
        return OVERDRAFT_FEE
    return 0


def account_id_valid(account_id):
    """Whether an account id is well formed."""
    return account_id.startswith("AC-")
