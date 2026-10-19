# ledger

A small Python package for a bank's back office (standard library only, Python 3.10+). Every amount is an integer
number of cents.

| Module | What it does |
| --- | --- |
| ledger/money.py | parsing, formatting, splitting and converting amounts |
| ledger/accounts.py | balances, transactions, overdraft fees, account ids |
| ledger/statements.py | statement totals and closing balances |
| ledger/text.py | memo text helpers |

Open work is on the ticket board under tickets/ (one file per ticket, with acceptance criteria).

When a ticket is finished, mark it done exactly once, from the repository root:

    python3 tools/done.py T03

Marking a ticket done may open follow-up tickets (done.py prints them and writes them to tickets/).

Quick tests, run from the repository root:

    python3 -m unittest discover -s tests
