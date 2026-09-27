"""Applying hunks to a list of lines, with offset search for hunks whose context moved."""
from .hunks import reverse_hunks
from .model import PatchError
from .search import locate
from .unified import format_hunks, parse_unified


def plan(lines, hunks):
    """Yield (position, offset) for each hunk in order: where its old lines sit in `lines` and how far that is from
    its header. Raises PatchError for the first hunk that does not apply."""
    floor = offset = 0
    for index, hunk in enumerate(hunks):
        try:
            hunk.check()
        except ValueError as error:
            raise PatchError(index, str(error)) from None
        old = hunk.old_lines()
        anchor = hunk.old_anchor()
        pos = locate(lines, old, anchor + offset, floor)
        if pos is None:
            raise PatchError(index, f"context not found for {hunk.header()}")
        offset = pos - anchor
        floor = pos + len(old)
        yield pos, offset


def apply_hunks(lines, hunks):
    """The lines after applying every hunk in order; the input is not modified."""
    result = list(lines)
    shift = 0
    for hunk, (pos, _) in zip(hunks, plan(result, hunks)):
        start = pos + shift
        result[start:start + hunk.old_count] = hunk.new_lines()
        shift += hunk.new_count - hunk.old_count
    return result


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
    return [offset for _, offset in plan(list(lines), hunks)]
