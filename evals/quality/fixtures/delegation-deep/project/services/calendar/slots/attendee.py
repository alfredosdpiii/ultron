"""An attendee: working hours, busy blocks, and a buffer kept free around every busy block."""
from .hours import WeeklyHours
from .intervals import IntervalSet
from .timeutil import DAY, day_start


class Attendee:
    """`busy` is an iterable of (start, end) UTC minutes; `buffer` minutes before and after each busy block are
    not free either (a meeting may not start right after or end right before a busy block).

    Free time is computed one UTC day at a time and kept until a change to the busy blocks affects that day.
    `version` counts changes to the busy blocks.
    """

    def __init__(self, name, hours, busy=(), buffer=0):
        if not isinstance(hours, WeeklyHours):
            raise TypeError("hours must be a WeeklyHours")
        if isinstance(buffer, bool) or not isinstance(buffer, int) or buffer < 0:
            raise ValueError("buffer must be a non-negative int")
        self.name = name
        self.hours = hours
        self.buffer = buffer
        self.version = 0
        self._busy = IntervalSet()
        self._free_by_day = {}
        for start, end in busy:
            self.add_busy(start, end)

    def add_busy(self, start, end):
        if end <= start:
            raise ValueError(f"busy block must have start < end, got {(start, end)!r}")
        self._busy.add(start, end)
        self.version += 1
        for day in range(day_start(start), end, DAY):
            self._free_by_day.pop(day, None)

    def clear_busy(self):
        self._busy = IntervalSet()
        self._free_by_day.clear()
        self.version += 1

    def busy(self):
        """The merged busy blocks, without buffers."""
        return self._busy.intervals()

    def blocked(self, start, end):
        """Minutes of [start, end) that are busy or inside a buffer."""
        nearby = self._busy.clip(start - self.buffer, end + self.buffer)
        return nearby.expand(self.buffer, self.buffer).clip(start, end)

    def _day_free(self, day):
        free = self._free_by_day.get(day)
        if free is None:
            free = self.hours.to_utc(day, day + DAY).subtract(self.blocked(day, day + DAY))
            self._free_by_day[day] = free
        return free

    def free(self, start, end):
        """Free minutes of the UTC window [start, end): working time that is not blocked."""
        if end <= start:
            return IntervalSet()
        pieces = []
        for day in range(day_start(start), end, DAY):
            pieces.extend(self._day_free(day).clip(start, end))
        return IntervalSet(pieces)

    def is_free(self, start, end):
        return end > start and self.free(start, end).contains(start, end)

    def __repr__(self):
        return f"Attendee({self.name!r}, busy={len(self._busy)} blocks, buffer={self.buffer})"
