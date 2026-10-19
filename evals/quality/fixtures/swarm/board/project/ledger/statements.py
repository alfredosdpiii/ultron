"""Statements: totals per period and closing balances."""


def monthly_totals(transactions):
    """Net amount per period: transactions are dicts with "date" ("YYYY-MM-DD") and a signed "amount"."""
    totals = {}
    for tx in transactions:
        key = tx["date"][:4]
        totals[key] = totals.get(key, 0) + tx["amount"]
    return totals


def closing_balance(opening, transactions):
    """The balance after all transactions (signed "amount")."""
    balance = opening
    for tx in transactions:
        balance += tx["amount"]
    return balance


def statement_lines(transactions):
    """One display line per transaction: "YYYY-MM-DD  memo  amount"."""
    return [f"{tx['date']}  {tx.get('memo', '')}  {tx['amount']}" for tx in transactions]
