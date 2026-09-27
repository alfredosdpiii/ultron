"""An attendee: working hours, busy blocks, and a buffer kept free around every busy block."""
from .hours import WeeklyHours
from .intervals import IntervalSet


class Attendee:
    """`busy` is an iterable of (start, end) UTC minutes; `buffer` minutes before and after each busy block are
    not free either (a meeting may not start right after or end right before a busy block)."""

    def __init__(self, name, hours, busy=(), buffer=0):
        if not isinstance(hours, WeeklyHours):
            raise TypeError("hours must be a WeeklyHours")
        if isinstance(buffer, bool) or not isinstance(buffer, int) or buffer < 0:
            raise ValueError("buffer must be a non-negative int")
        self.name = name
        self.hours = hours
        self.buffer = buffer
        self._busy = IntervalSet()
        for start, end in busy:
            self.add_busy(start, end)

    def add_busy(self, start, end):
        if end <= start:
            raise ValueError(f"busy block must have start < end, got {(start, end)!r}")
        self._busy = self._busy.union(IntervalSet.span(start, end))

    def clear_busy(self):
        self._busy = IntervalSet()

    def busy(self):
        """The merged busy blocks, without buffers."""
        return self._busy.intervals()

    def blocked(self, start, end):
        """Minutes of [start, end) that are busy or inside a buffer."""
        nearby = self._busy.clip(start, end)
        return nearby.expand(self.buffer, self.buffer).clip(start, end)

    def free(self, start, end):
        """Free minutes of the UTC window [start, end): working time that is not blocked."""
        if end <= start:
            return IntervalSet()
        return self.hours.to_utc(start, end).subtract(self.blocked(start, end))

    def is_free(self, start, end):
        return end > start and self.free(start, end).contains(start, end)

    def __repr__(self):
        return f"Attendee({self.name!r}, busy={len(self._busy)} blocks, buffer={self.buffer})"
