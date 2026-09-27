"""Finding time when every attendee is free."""
from .intervals import IntervalSet
from .timeutil import DAY, WEEK, align_up

DEFAULT_HORIZON = 4 * WEEK


def _check_attendees(attendees):
    attendees = list(attendees)
    if not attendees:
        raise ValueError("at least one attendee is needed")
    return attendees


def _positive(name, value):
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ValueError(f"{name} must be a positive int, got {value!r}")
    return value


def common_free(attendees, start, end):
    """Minutes of [start, end) when every attendee is free."""
    attendees = _check_attendees(attendees)
    result = IntervalSet.span(start, end)
    for attendee in attendees:
        result = result.intersect(attendee.free(start, end))
        if not result:
            break
    return result


def free_slots(attendees, start, end, min_length=1):
    """Maximal common free ranges inside [start, end) that are at least `min_length` minutes long."""
    _positive("min_length", min_length)
    return common_free(attendees, start, end).longer_than(min_length).intervals()


def find_slot(attendees, duration, earliest, granularity=1, horizon=DEFAULT_HORIZON):
    """First start t >= earliest, a multiple of `granularity`, with [t, t + duration) free for everyone and
    t + duration <= earliest + horizon. Returns None when there is no such start."""
    attendees = _check_attendees(attendees)
    _positive("duration", duration)
    _positive("granularity", granularity)
    _positive("horizon", horizon)
    limit = earliest + horizon
    if align_up(earliest, granularity) + duration > limit:
        return None
    grid = earliest
    chunk = earliest
    while chunk < limit:
        chunk_end = min(chunk + DAY, limit)
        window_end = min(chunk_end + duration, limit)
        for low, high in common_free(attendees, chunk, window_end):
            if low >= chunk_end:
                break
            candidate = low + (grid - low) % granularity
            if candidate >= chunk_end:
                continue
            if candidate + duration <= high:
                return candidate
        chunk = chunk_end
    return None


def find_slots(attendees, duration, earliest, count, granularity=1, horizon=DEFAULT_HORIZON):
    """Up to `count` non-overlapping slots, each the first one found after the previous slot ends."""
    _positive("count", count)
    found = []
    cursor = earliest
    limit = earliest + horizon
    while len(found) < count and cursor < limit:
        start = find_slot(attendees, duration, cursor, granularity, limit - cursor)
        if start is None:
            break
        found.append((start, start + duration))
        cursor = start + duration
    return found


def first_common_day(attendees, earliest, min_minutes, horizon=DEFAULT_HORIZON):
    """Start of the first UTC day (from the day of `earliest`) with at least `min_minutes` common free minutes."""
    _positive("min_minutes", min_minutes)
    day = earliest - earliest % DAY
    while day < earliest + horizon:
        if common_free(attendees, max(day, earliest), day + DAY).total() >= min_minutes:
            return day
        day += DAY
    return None
