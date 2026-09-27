"""Meeting-slot finding over integer UTC minutes (minute 0 is Monday 00:00 UTC)."""
from .attendee import Attendee
from .hours import WeeklyHours
from .intervals import IntervalSet
from .search import DEFAULT_HORIZON, common_free, find_slot, find_slots, first_common_day, free_slots
from .timeutil import DAY, WEEK, align_up, format_minute, weekday_of

__all__ = [
    "Attendee",
    "DAY",
    "DEFAULT_HORIZON",
    "IntervalSet",
    "WEEK",
    "WeeklyHours",
    "align_up",
    "common_free",
    "find_slot",
    "find_slots",
    "first_common_day",
    "format_minute",
    "free_slots",
    "weekday_of",
]
