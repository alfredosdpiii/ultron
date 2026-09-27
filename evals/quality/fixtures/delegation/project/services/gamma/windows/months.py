"""Month arithmetic."""
import calendar
from datetime import date


def days_in_month(year, month):
    return calendar.monthrange(year, month)[1]


def add_months(day, months):
    """The same day of the month `months` months away, clamped to the end of the target month."""
    month = day.month + months
    year = day.year + (month - 1) // 12
    month = month % 12
    return date(year, month, min(day.day, days_in_month(year, month)))
