# gamma: reporting date windows

Date windows for reports. A window is a half-open range of dates `(start, end)`: it contains `start` and every day
before `end`, but not `end` itself. All values are `datetime.date`.

## Kinds

`parse_kind(text)` accepts, ignoring case and surrounding whitespace:

- `day`, `week`, `month`, `quarter`: returns `(unit, 1)`.
- `trailing-N` for an integer `1 <= N <= 366`: returns `("trailing", N)`.

Anything else raises `ValueError`.

## window(kind, anchor)

The window of the given kind that contains `anchor`:

- `day`: just `anchor`.
- `week`: the ISO week of `anchor`, Monday to Sunday.
- `month`: the calendar month of `anchor`.
- `quarter`: the calendar quarter of `anchor` (January-March, April-June, July-September, October-December).
- `trailing-N`: the N days ending with `anchor`, inclusive (`trailing-1` is just `anchor`).

## shift(kind, anchor, k)

The window of the same kind `k` periods later (`k` may be negative or zero): for `day`, `week` and `trailing-N`, the
window of `anchor` moved by `k` times its length in days; for `month` and `quarter`, the month or quarter `k`
periods away (`shift("month", date(2026, 1, 31), 1)` is February 2026).

## add_months(day, months)

The same day of the month `months` months later (earlier if negative), clamped to the last day of the target month
(`add_months(date(2026, 1, 31), 1) == date(2026, 2, 28)`), with the year carried as needed in both directions.
