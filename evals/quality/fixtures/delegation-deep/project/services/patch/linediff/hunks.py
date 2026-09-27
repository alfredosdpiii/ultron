"""Grouping an edit script into hunks with context, and reversing hunks."""
import functools

from .lcs import diff_lines
from .model import Hunk


def _change_blocks(ops):
    """Maximal change regions as [i1, i2, j1, j2]: a delete op and the insert op after it form one region."""
    blocks = []
    for op in ops:
        if op.tag == "equal":
            continue
        if blocks and blocks[-1][1] == op.i1 and blocks[-1][3] == op.j1:
            blocks[-1][1], blocks[-1][3] = op.i2, op.j2
        else:
            blocks.append([op.i1, op.i2, op.j1, op.j2])
    return blocks


@functools.lru_cache(maxsize=128)
def _analysis(a, b):
    """Edit script and change regions of two files (as tuples). Callers often ask for several contexts of one pair."""
    ops = diff_lines(a, b)
    return ops, _change_blocks(ops)


def _spans(blocks, context):
    """[i1, i2, j1, j2] spans covering the change regions that share a hunk: regions whose context would overlap
    or touch are joined."""
    spans = []
    for block in blocks:
        if spans and block[0] - spans[-1][1] <= 2 * context:
            spans[-1][1], spans[-1][3] = block[1], block[3]
        else:
            spans.append(block)
    return spans


def _start(first_index, count):
    return first_index + 1 if count else first_index


def _build(a, b, ops, span, context):
    i1, i2, j1, j2 = span
    lead = min(context, i1, j1)
    trail = min(context, len(a) - i2, len(b) - j2)
    lo, hi = i1 - lead, i2 + trail
    nlo = j1 - lead
    lines = []
    for op in ops:
        if op.tag == "equal":
            lines.extend((" ", a[k]) for k in range(max(op.i1, lo), min(op.i2, hi)))
        elif op.tag == "delete":
            if i1 <= op.i1 and op.i2 <= i2:
                lines.extend(("-", a[k]) for k in range(op.i1, op.i2))
        elif j1 <= op.j1 and op.j2 <= j2:
            lines.extend(("+", b[k]) for k in range(op.j1, op.j2))
    old_count = hi - lo
    new_count = sum(1 for tag, _ in lines if tag != "-")
    return Hunk(_start(lo, old_count), old_count, _start(nlo, new_count), new_count, lines)


def make_hunks(a, b, context=3):
    """Hunks turning a into b, each with up to `context` unchanged lines around its changes."""
    if context < 0:
        raise ValueError("context must not be negative")
    a, b = tuple(a), tuple(b)
    ops, blocks = _analysis(a, b)
    return [_build(a, b, ops, span, context) for span in _spans(blocks, context)]


def _reorder(lines):
    """Within every run of changed lines, '-' lines first, then '+' lines, each keeping its order."""
    out, run = [], []
    for tag, text in lines + [(" ", None)]:
        if tag == " ":
            out.extend(line for line in run if line[0] == "-")
            out.extend(line for line in run if line[0] == "+")
            run = []
            if text is not None:
                out.append((tag, text))
        else:
            run.append((tag, text))
    return out


def reverse_hunks(hunks):
    """Hunks that undo `hunks`: old and new sides swapped."""
    flipped = {" ": " ", "-": "+", "+": "-"}
    return [
        Hunk(h.new_start, h.new_count, h.old_start, h.old_count, _reorder([(flipped[tag], text) for tag, text in h.lines]))
        for h in hunks
    ]


def diffstat(hunks):
    """(lines added, lines removed) over all hunks."""
    added = sum(1 for h in hunks for tag, _ in h.lines if tag == "+")
    removed = sum(1 for h in hunks for tag, _ in h.lines if tag == "-")
    return added, removed
