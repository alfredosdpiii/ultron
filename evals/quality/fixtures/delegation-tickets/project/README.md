# shop

A small Python package for a web shop's back office (standard library only, Python 3.10+). Every amount is an
integer number of cents.

| Module | What it does |
| --- | --- |
| shop/pricing.py | rounding, discounts, tax, volume prices, price formatting |
| shop/inventory.py | stock levels: reservations, restocking, low-stock lists |
| shop/orders.py | order lines, shipping and the total the customer pays |
| shop/text.py | product-name helpers: slugs and shortened labels |
| shop/report.py | sales reports: revenue per SKU, best sellers, report rows |

Open work is described in tickets/ (one file per ticket, with acceptance criteria).

Quick tests, run from the repository root:

    python3 -m unittest discover -s tests
