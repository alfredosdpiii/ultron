"""Tick arithmetic for the buckets. Time is an integer tick count that callers pass in."""


def advance(last, now):
    """(elapsed ticks, new last tick) for a bucket that last saw tick `last` and now sees tick `now`."""
    return now - last, now
