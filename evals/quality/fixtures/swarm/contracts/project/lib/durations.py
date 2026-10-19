"""Human durations: "90s", "1h30m", "2d 4h" -> seconds."""
import re

UNITS = {"d": 86400, "h": 3600, "m": 60, "s": 1}


def parse_duration(text):
    """Seconds in a duration made of <number><unit> parts (units d, h, m, s)."""
    match = re.match(r"\s*(\d+)([dhms])", text)
    if not match:
        raise ValueError(f"not a duration: {text!r}")
    return int(match.group(1)) * UNITS[match.group(2)]
