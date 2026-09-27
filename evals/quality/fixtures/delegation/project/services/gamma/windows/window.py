"""Half-open date windows."""
from datetime import date, timedelta

from .months import add_months
from .parse import parse_kind


def _start(unit, n, anchor):
    if unit == "day":
        return anchor
    if unit == "week":
        return anchor - timedelta(days=(anchor.weekday() + 1) % 7)
    if unit == "month":
        return anchor.replace(day=1)
    if unit == "quarter":
        return date(anchor.year, (anchor.month - 1) // 3 * 3 + 1, 1)
    return anchor - timedelta(days=n - 1)


def _end(unit, n, start):
    if unit == "month":
        return add_months(start, 1)
    if unit == "quarter":
        return add_months(start, 3)
    return start + timedelta(days={"day": 1, "week": 7}.get(unit, n))


def window(kind, anchor):
    unit, n = parse_kind(kind)
    start = _start(unit, n, anchor)
    return start, _end(unit, n, start)


def shift(kind, anchor, k):
    unit, n = parse_kind(kind)
    start = _start(unit, n, anchor)
    if unit == "month":
        start = add_months(start, k)
    elif unit == "quarter":
        start = add_months(start, 3 * k)
    else:
        start = start + timedelta(days=k * {"day": 1, "week": 7}.get(unit, n))
    return start, _end(unit, n, start)
