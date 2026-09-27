"""Weekly working hours in an attendee's local time, converted to UTC minutes on demand."""
from .intervals import IntervalSet
from .timeutil import DAY, check_offset, parse_clock, weekday_index


class WeeklyHours:
    """Working ranges per local weekday plus a fixed UTC offset (local time = UTC + utc_offset minutes).

    `ranges` maps a weekday (0-6 or a name such as "mon") to a list of (start, end) clock values, each an int
    number of minutes since local midnight or an "HH:MM" string, with 0 <= start < end <= 1440. Weekdays that are
    missing have no working time. Ranges of one weekday may overlap; they are merged.
    """

    def __init__(self, ranges, utc_offset=0):
        self.utc_offset = check_offset(utc_offset)
        self._by_day = {day: IntervalSet() for day in range(7)}
        for day, spans in dict(ranges).items():
            index = weekday_index(day)
            pairs = []
            for span in spans:
                start, end = span
                start, end = parse_clock(start), parse_clock(end)
                if start >= end:
                    raise ValueError(f"working range must have start < end, got {span!r}")
                pairs.append((start, end))
            self._by_day[index] = self._by_day[index].union(IntervalSet(pairs))

    @classmethod
    def office(cls, start="09:00", end="17:00", days=(0, 1, 2, 3, 4), utc_offset=0):
        """The same single range on each of `days`."""
        return cls({day: [(start, end)] for day in days}, utc_offset)

    def local_ranges(self, weekday):
        """The merged local ranges of one weekday as (start, end) minutes since midnight."""
        return self._by_day[weekday_index(weekday)].intervals()

    def weekly_minutes(self):
        return sum(spans.total() for spans in self._by_day.values())

    def to_utc(self, start, end):
        """The working time inside the UTC window [start, end) as an IntervalSet of UTC minutes."""
        if end <= start:
            return IntervalSet()
        local_start = start + self.utc_offset
        local_end = end + self.utc_offset
        first_day = local_start // DAY
        last_day = (local_end - 1) // DAY
        pairs = []
        for day in range(first_day, last_day + 1):
            spans = self._by_day[day % 7]
            base = day * DAY - self.utc_offset
            pairs.extend((base + low, base + high) for low, high in spans)
        return IntervalSet(pairs).clip(start, end)
