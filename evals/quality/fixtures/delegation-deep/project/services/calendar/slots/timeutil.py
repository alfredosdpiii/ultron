"""Minute arithmetic. Minute 0 is Monday 00:00 UTC; days and weeks are whole multiples of DAY and WEEK."""

DAY = 24 * 60
WEEK = 7 * DAY
WEEKDAYS = ("mon", "tue", "wed", "thu", "fri", "sat", "sun")
MIN_OFFSET = -12 * 60
MAX_OFFSET = 14 * 60


def weekday_of(minute):
    """Weekday (0 = Monday) of the day holding `minute`."""
    return (minute // DAY) % 7


def day_start(minute):
    """First minute of the day holding `minute`."""
    return minute - minute % DAY


def weekday_index(day):
    """Accepts 0-6 or a three-letter name (any case) and returns 0-6."""
    if isinstance(day, str):
        key = day.strip().lower()[:3]
        if key not in WEEKDAYS:
            raise ValueError(f"unknown weekday {day!r}")
        return WEEKDAYS.index(key)
    if isinstance(day, bool) or not isinstance(day, int) or not 0 <= day <= 6:
        raise ValueError(f"weekday must be 0-6 or a name, got {day!r}")
    return day


def parse_clock(value):
    """Minutes since midnight from an int or an "HH:MM" string ("24:00" is the end of the day)."""
    if isinstance(value, bool):
        raise ValueError("clock value must be an int or HH:MM")
    if isinstance(value, int):
        minutes = value
    elif isinstance(value, str) and ":" in value:
        hours, _, rest = value.strip().partition(":")
        if not (hours.isdigit() and rest.isdigit() and len(rest) == 2):
            raise ValueError(f"bad clock value {value!r}")
        if int(rest) >= 60:
            raise ValueError(f"bad clock value {value!r}")
        minutes = int(hours) * 60 + int(rest)
    else:
        raise ValueError(f"bad clock value {value!r}")
    if not 0 <= minutes <= DAY:
        raise ValueError(f"clock value out of range: {value!r}")
    return minutes


def check_offset(offset):
    if isinstance(offset, bool) or not isinstance(offset, int) or not MIN_OFFSET <= offset <= MAX_OFFSET:
        raise ValueError(f"utc_offset must be an int in [{MIN_OFFSET}, {MAX_OFFSET}], got {offset!r}")
    return offset


def align_up(value, granularity):
    """Smallest multiple of `granularity` that is >= `value`."""
    return -(-value // granularity) * granularity


def format_minute(minute):
    """A readable label such as "w1 tue 09:30" (week number, weekday, clock)."""
    week, rest = divmod(minute, WEEK)
    day, clock = divmod(rest, DAY)
    return f"w{week} {WEEKDAYS[day]} {clock // 60:02d}:{clock % 60:02d}"
