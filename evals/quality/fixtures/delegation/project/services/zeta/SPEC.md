# zeta: customer CSV normalization

`normalize(text)` takes a customer export as CSV text and returns it cleaned up, as CSV text.

## Reading

- Standard CSV (Python's `csv` module dialect: commas, double-quote quoting).
- A leading byte order mark (U+FEFF) is ignored.
- Rows whose cells are all empty or whitespace are skipped. The first remaining row is the header.
- Header names are normalized: surrounding whitespace removed, lowercased, and each inner run of whitespace
  replaced by one underscore (`" Customer  Name "` becomes `customer_name`).
- A row shorter than the header is padded with empty cells; cells beyond the header's length are dropped.

## Cells

Every cell has its surrounding whitespace removed. Some columns (by normalized header name) are normalized further:

- `email`: lowercased.
- `name`: inner runs of whitespace become one space.
- `amount`: a money amount written as `1234.5`, `1,234.50`, `$1,234.50`, `-$3`, `-3` or, as accountants write
  negative amounts, in parentheses: `(12.00)` or `($1,200.00)` mean minus 12 and minus 1200. The output is the
  amount with exactly two decimals, rounded half up, with a leading `-` when negative and no `$` or thousands
  separators: `1234.50`, `-3.00`, `-12.00`. Zero is `0.00`, never `-0.00`. An empty cell stays empty. Anything else
  raises `ValueError`.

## Duplicates

When there is an `email` column, rows are de-duplicated by their normalized email: the first row with a given email
is kept, later ones are dropped, and the order of the kept rows is unchanged. Rows with an empty email are always
kept.

## Output

CSV written with Python's `csv.writer` and `\n` line endings: the normalized header, then the rows.
