"""Finding where a block of lines sits in a file: the expected position first, then the nearest match."""

_INDEX_LIMIT = 64
_indexes = {}


def _line_index(source):
    """Positions of every distinct line of `source`, in increasing order. Reused while the same file is patched."""
    key = (id(source), len(source))
    index = _indexes.get(key)
    if index is None:
        index = {}
        for pos, line in enumerate(source):
            index.setdefault(line, []).append(pos)
        if len(_indexes) >= _INDEX_LIMIT:
            _indexes.clear()
        _indexes[key] = index
    return index


def _nearest_first(candidates, expected):
    """Candidates ordered by distance from `expected`; of two equally near, the earlier first."""
    return sorted(candidates, key=lambda pos: (abs(pos - expected), pos))


def matches_at(source, pos, old):
    return 0 <= pos <= len(source) - len(old) and list(source[pos:pos + len(old)]) == list(old)


def locate(source, old, expected, floor=0):
    """Start index of the block `old` in `source`, at or after `floor`, nearest to `expected`; None when absent."""
    hi = len(source) - len(old)
    if floor > hi:
        return None
    if floor <= expected and matches_at(source, expected, old):
        return expected
    if not old:
        return min(max(expected, floor), hi)
    starts = [pos for pos in _line_index(source).get(old[0], ()) if floor <= pos <= hi]
    for pos in _nearest_first(starts, expected):
        if matches_at(source, pos, old):
            return pos
    return None


def occurrences(source, old):
    """Every start index of the block `old` in `source`."""
    if not old:
        return list(range(len(source) + 1))
    return [pos for pos in _line_index(source).get(old[0], ()) if matches_at(source, pos, old)]
