"""Environment strings to typed values."""
import re

_INT = re.compile(r"^\d+$")
_FLOAT = re.compile(r"^-?\d+\.\d+$")


def coerce(text):
    stripped = text.strip()
    lowered = stripped.lower()
    if lowered == "true":
        return True
    if lowered == "false":
        return False
    if lowered == "null":
        return None
    if _INT.match(stripped):
        return int(stripped)
    if _FLOAT.match(stripped):
        return float(stripped)
    return text
