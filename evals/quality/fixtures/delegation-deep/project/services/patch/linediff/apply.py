"""Applying hunks to a list of lines, with offset search for hunks whose context moved."""
from .hunks import reverse_hunks
from .model import PatchError
from .unified import format_hunks, parse_unified


def _positions(expected, lo, hi):
    """Every start position from lo to hi (inclusive), nearest to `expected` first."""
    if lo > hi:
        return
    reach = max(expected - lo, hi - expected)
    for step in range(reach + 1):
        for pos in (expected + step, expected - step) if step else (expected,):
            if lo <= pos <= hi:
                yield pos


def _matches(source, pos, old):
    return source[pos:pos + len(old)] == old


def locate(source, old, expected, floor=0):
    """Start index of `old` in `source` at or after `floor`, nearest to `expected`; None when absent."""
    for pos in _positions(expected, floor, len(source) - len(old)):
        if _matches(source, pos, old):
            return pos
    return None


def plan(lines, hunks):
    """Where each hunk applies: a list of (position, offset) pairs. Raises PatchError for the first that does not."""
    source = list(lines)
    placements = []
    floor = offset = 0
    for index, hunk in enumerate(hunks):
        try:
            hunk.check()
        except ValueError as error:
            raise PatchError(index, str(error)) from None
        old = hunk.old_lines()
        anchor = hunk.old_anchor()
        pos = locate(source, old, anchor + offset, floor)
        if pos is None:
            raise PatchError(index, f"context not found for {hunk.header()}")
        placements.append((pos, pos - anchor))
        floor = pos + len(old)
        offset = pos - anchor
    return placements


def apply_hunks(lines, hunks):
    """The lines after applying every hunk in order; the input is not modified."""
    source = list(lines)
    out = []
    floor = 0
    for hunk, (pos, _) in zip(hunks, plan(source, hunks)):
        out.extend(source[floor:pos])
        out.extend(hunk.new_lines())
        floor = pos + hunk.old_count
    out.extend(source[floor:])
    return out


def apply_patch(lines, text):
    """Apply a unified diff given as text."""
    return apply_hunks(lines, parse_unified(text))


def reverse_patch(text):
    """The unified diff that undoes `text` (file names swapped)."""
    head = text.split("\n")[:2]
    if len(head) == 2 and head[0].startswith("--- ") and head[1].startswith("+++ "):
        from_name, to_name = head[0][4:], head[1][4:]
    else:
        from_name, to_name = "a", "b"
    return format_hunks(reverse_hunks(parse_unified(text)), to_name, from_name)


def offsets(lines, hunks):
    """How far each hunk moved from its header position (0 when it applied where the header says)."""
    return [offset for _, offset in plan(lines, hunks)]
