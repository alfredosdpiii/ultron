"""Unified diff text: formatting hunks and parsing them back."""
import re

from .hunks import make_hunks
from .model import Hunk

_HEADER = re.compile(r"^@@ -(\d+),(\d+) \+(\d+),(\d+) @@(?: .*)?$")


def format_hunk(hunk):
    """The hunk's header line followed by one line per body line (tag, then text), without newlines."""
    return [hunk.header()] + [tag + text for tag, text in hunk.lines]


def format_hunks(hunks, from_name="a", to_name="b"):
    """A unified diff for `hunks`; the empty string when there are none."""
    if not hunks:
        return ""
    out = [f"--- {from_name}", f"+++ {to_name}"]
    for hunk in hunks:
        hunk.check()
        out.extend(format_hunk(hunk))
    return "\n".join(out) + "\n"


def format_unified(a, b, context=3, from_name="a", to_name="b"):
    """Unified diff turning the lines `a` into the lines `b`."""
    return format_hunks(make_hunks(a, b, context), from_name, to_name)


def _split(text):
    if not text:
        return []
    lines = text.split("\n")
    if lines[-1] == "":
        lines.pop()
    return lines


def parse_unified(text):
    """Hunks of a unified diff, in order. Raises ValueError on malformed input (see SPEC.md)."""
    hunks = []
    current = None
    old_left = new_left = 0
    for number, line in enumerate(_split(text), 1):
        if line.startswith(("--- ", "+++ ")):
            continue
        if old_left or new_left:
            tag, body = line[:1], line[1:]
            if tag == " ":
                old_left -= 1
                new_left -= 1
            elif tag == "-":
                old_left -= 1
            elif tag == "+":
                new_left -= 1
            else:
                raise ValueError(f"line {number}: expected a hunk body line, got {line!r}")
            if old_left < 0 or new_left < 0:
                raise ValueError(f"line {number}: hunk body longer than its header {current.header()}")
            current.lines.append((tag, body))
            continue
        match = _HEADER.match(line)
        if match is None:
            raise ValueError(f"line {number}: unexpected {line!r}")
        old_start, old_count, new_start, new_count = (int(group) for group in match.groups())
        current = Hunk(old_start, old_count, new_start, new_count, [])
        hunks.append(current)
        old_left, new_left = old_count, new_count
    if old_left or new_left:
        raise ValueError(f"truncated hunk {current.header()}")
    for hunk in hunks:
        hunk.check()
    return hunks
