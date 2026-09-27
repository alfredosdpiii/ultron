"""Minimal line diff: a longest-common-subsequence edit script."""
from .model import Op


def _common_prefix(a, b):
    n = 0
    limit = min(len(a), len(b))
    while n < limit and a[n] == b[n]:
        n += 1
    return n


def _common_suffix(a, b, prefix):
    n = 0
    limit = min(len(a), len(b)) - prefix
    while n < limit and a[len(a) - 1 - n] == b[len(b) - 1 - n]:
        n += 1
    return n


def lcs_table(a, b):
    """table[i][j] is the length of the longest common subsequence of a[i:] and b[j:]."""
    table = [[0] * (len(b) + 1) for _ in range(len(a) + 1)]
    for i in range(len(a) - 1, -1, -1):
        row, below = table[i], table[i + 1]
        for j in range(len(b) - 1, -1, -1):
            if a[i] == b[j]:
                row[j] = below[j + 1] + 1
            else:
                row[j] = max(below[j], row[j + 1])
    return table


def lcs_length(a, b):
    return lcs_table(a, b)[0][0] if a and b else 0


def _steps(a, b):
    """Per-line steps ('=', '-', '+') of one longest common subsequence alignment."""
    table = lcs_table(a, b)
    i = j = 0
    steps = []
    while i < len(a) or j < len(b):
        if i < len(a) and j < len(b) and a[i] == b[j] and table[i][j] == table[i + 1][j + 1] + 1:
            steps.append("=")
            i += 1
            j += 1
        elif j >= len(b) or (i < len(a) and table[i + 1][j] >= table[i][j + 1]):
            steps.append("-")
            i += 1
        else:
            steps.append("+")
            j += 1
    return steps


def _group(steps, i, j):
    """Collapse steps into ops: runs of '=' become one equal op; each change region one delete then one insert."""
    ops = []
    k = 0
    while k < len(steps):
        if steps[k] == "=":
            start = k
            while k < len(steps) and steps[k] == "=":
                k += 1
            n = k - start
            ops.append(Op("equal", i, i + n, j, j + n))
            i += n
            j += n
            continue
        deleted = inserted = 0
        while k < len(steps) and steps[k] != "=":
            if steps[k] == "-":
                deleted += 1
            else:
                inserted += 1
            k += 1
        if deleted:
            ops.append(Op("delete", i, i + deleted, j, j))
        if inserted:
            ops.append(Op("insert", i + deleted, i + deleted, j, j + inserted))
        i += deleted
        j += inserted
    return ops


def diff_lines(a, b):
    """Minimal edit script turning a into b (see SPEC.md for the shape of the ops)."""
    a, b = list(a), list(b)
    prefix = _common_prefix(a, b)
    suffix = _common_suffix(a, b, prefix)
    middle_a = a[prefix:len(a) - suffix]
    middle_b = b[prefix:len(b) - suffix]
    steps = ["="] * prefix + _steps(middle_a, middle_b) + ["="] * suffix
    return _group(steps, 0, 0)


def similarity(a, b):
    """2 * common / (len(a) + len(b)); 1.0 for two empty files."""
    total = len(a) + len(b)
    if total == 0:
        return 1.0
    common = sum(op.old_len() for op in diff_lines(a, b) if op.tag == "equal")
    return 2 * common / total
