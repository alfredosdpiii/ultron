# beta: invoice totals

Builds an invoice from order lines with exact decimal money (Python `Decimal`, never floats).

## Inputs

- `Line(description, unit_price, quantity, tax_rate)`: `unit_price` is a decimal string (it may have more than two
  decimal places, e.g. `"0.125"`), `quantity` a non-negative integer, `tax_rate` a percentage as a decimal string
  (`"20"`, `"7.5"`).
- `build_invoice(lines, discount_percent="0")`: `discount_percent` is an invoice-level percentage (decimal string).

## Rounding

Every amount is rounded to whole cents with **round half up** (half a cent goes away from zero:
`0.125 -> 0.13`, `0.135 -> 0.14`, `2.675 -> 2.68`).

## Computation

1. Line net: `round(unit_price * quantity)`.
2. Subtotal: the sum of the line nets.
3. Discount total: `round(subtotal * discount_percent / 100)`.
4. The discount total is allocated to the lines in proportion to their nets, in whole cents, so that the line
   discounts add up to exactly the discount total (largest remainder method): each line first gets the whole cents
   of its exact proportional share (rounded down), then the cents still left over go one each to the lines with the
   largest fractional remainders; equal remainders go to the earlier line first. If the subtotal is zero every line
   discount is zero.
5. Line tax: `round((net - line discount) * tax_rate / 100)`. Tax is charged on the discounted amount.
6. Line total: `net - line discount + line tax`.
7. Invoice `subtotal`, `discount`, `tax` and `total` are the sums over the lines.

## Output

`build_invoice` returns a dict: `lines` (a list of dicts with `description`, `net`, `discount`, `tax`, `total`) and
`subtotal`, `discount`, `tax`, `total`. Every amount is a string with exactly two decimals (`"12.30"`).
