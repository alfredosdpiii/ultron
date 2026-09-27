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
        self._capped_from = (self.cap // self.base).bit_length()

    def delay(self, attempt):
        check_int("attempt", attempt, 0)
        if attempt >= self._capped_from:
            return self.cap
        return self.base << attempt

    def schedule(self):
        """Delays for the consecutive failed attempts of one job: attempt 0, 1, 2, ..."""
        attempt = 0
        while True:
            yield self.delay(attempt)
            attempt += 1

    def delays(self, count):
        """The first `count` delays, for display and planning."""
        return [self.delay(attempt) for attempt in range(count)]

    def total(self, count):
        """Ticks spent waiting across `count` consecutive failed attempts."""
        return sum(self.delays(count))

    def __repr__(self):
        return f"Backoff(base={self.base}, cap={self.cap})"
