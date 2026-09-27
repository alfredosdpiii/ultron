"""Retry delays with exponential backoff."""
from .job import check_int


class Backoff:
    """Delay before a retry: `base * 2**attempt` ticks after the failed attempt, never more than `cap` ticks.

    `attempt` counts from 0 (the first attempt of a job). Doublings are computed with shifts; once the doubled
    delay would pass the cap, every later attempt waits exactly `cap` ticks.
    """

    def __init__(self, base=1, cap=16):
        self.base = check_int("backoff_base", base, 1)
        self.cap = check_int("backoff_cap", cap, 1)
        self._capped_from = (self.cap // self.base).bit_length() - 1

    def delay(self, attempt):
        check_int("attempt", attempt, 0)
        if attempt >= self._capped_from:
            return self.cap
        return self.base << attempt

    def delays(self, count):
        """The first `count` delays, for display and planning."""
        return [self.delay(attempt) for attempt in range(count)]

    def total(self, count):
        """Ticks spent waiting across `count` consecutive failed attempts."""
        return sum(self.delays(count))

    def __repr__(self):
        return f"Backoff(base={self.base}, cap={self.cap})"
