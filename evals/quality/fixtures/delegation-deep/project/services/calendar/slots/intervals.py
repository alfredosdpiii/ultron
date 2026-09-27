"""Sets of half-open integer intervals [start, end).

An IntervalSet is kept in canonical form: its intervals are sorted, non-empty, and separated by gaps (two intervals
never overlap or touch). Set operations return new sets in canonical form; `add` grows a set in place.
"""
from bisect import bisect_left, bisect_right


def _normalize(pairs):
    """Sort, drop empty pairs, and merge overlapping or touching pairs."""
    cleaned = sorted((int(start), int(end)) for start, end in pairs if end > start)
    merged = []
    for start, end in cleaned:
        if merged and start <= merged[-1][1]:
            if end > merged[-1][1]:
                merged[-1][1] = end
        else:
            merged.append([start, end])
    return [(start, end) for start, end in merged]


class IntervalSet:
    """A set of integers given as half-open intervals."""

    __slots__ = ("_items", "_starts")

    def __init__(self, pairs=()):
        self._items = _normalize(pairs)
        self._starts = [start for start, _ in self._items]

    @classmethod
    def _canonical(cls, items):
        result = cls.__new__(cls)
        result._items = items
        result._starts = [start for start, _ in items]
        return result

    @classmethod
    def span(cls, start, end):
        """The set holding every integer of [start, end) (empty when end <= start)."""
        return cls([(start, end)])

    def intervals(self):
        """The canonical intervals as a list of (start, end) tuples."""
        return list(self._items)

    def __iter__(self):
        return iter(self._items)

    def __len__(self):
        return len(self._items)

    def __bool__(self):
        return bool(self._items)

    def __eq__(self, other):
        return isinstance(other, IntervalSet) and self._items == other._items

    def __repr__(self):
        return f"IntervalSet({self._items!r})"

    def total(self):
        """Number of integers in the set."""
        return sum(end - start for start, end in self._items)

    def bounds(self):
        """(first, end) of the whole set, or None when it is empty."""
        if not self._items:
            return None
        return self._items[0][0], self._items[-1][1]

    def contains(self, start, end):
        """True when every integer of [start, end) is in the set. Requires start < end."""
        if start >= end:
            raise ValueError("contains() needs a non-empty range")
        index = bisect_right(self._starts, start) - 1
        return index >= 0 and self._items[index][1] >= end

    def contains_point(self, value):
        index = bisect_right(self._starts, value) - 1
        return index >= 0 and value < self._items[index][1]

    def union(self, other):
        """Integers in either set."""
        if not other._items:
            return IntervalSet._canonical(self._items)
        if not self._items:
            return IntervalSet._canonical(other._items)
        left, right = self._items, other._items
        i = j = 0
        out = []
        while i < len(left) or j < len(right):
            if j >= len(right) or (i < len(left) and left[i][0] <= right[j][0]):
                start, end = left[i]
                i += 1
            else:
                start, end = right[j]
                j += 1
            if out and start <= out[-1][1]:
                if end > out[-1][1]:
                    out[-1] = (out[-1][0], end)
            else:
                out.append((start, end))
        return IntervalSet._canonical(out)

    def add(self, start, end):
        """Adds every integer of [start, end) to this set in place (nothing happens when end <= start)."""
        if end <= start:
            return
        ends = [high for _, high in self._items]
        low_index = bisect_left(ends, start)
        high_index = bisect_right(self._starts, end)
        if low_index < high_index:
            start = min(start, self._items[low_index][0])
            end = max(end, self._items[high_index - 1][1])
        self._items[low_index:high_index] = [(start, end)]
        self._starts[low_index:high_index] = [start]

    def copy(self):
        return IntervalSet._canonical(list(self._items))

    def intersect(self, other):
        """Integers in both sets."""
        left, right = self._items, other._items
        i = j = 0
        out = []
        while i < len(left) and j < len(right):
            low = max(left[i][0], right[j][0])
            high = min(left[i][1], right[j][1])
            if low < high:
                out.append((low, high))
            if left[i][1] < right[j][1]:
                i += 1
            else:
                j += 1
        return IntervalSet._canonical(out)

    def subtract(self, other):
        """Integers in this set but not in `other`."""
        cuts = other._items
        out = []
        j = 0
        for start, end in self._items:
            while j < len(cuts) and cuts[j][1] <= start:
                j += 1
            cursor = start
            k = j
            while k < len(cuts) and cuts[k][0] < end:
                if cuts[k][0] > cursor:
                    out.append((cursor, cuts[k][0]))
                cursor = max(cursor, cuts[k][1])
                if cursor >= end:
                    break
                k += 1
            if cursor < end:
                out.append((cursor, end))
        return IntervalSet._canonical(out)

    def clip(self, start, end):
        """The part of the set inside [start, end)."""
        return self.intersect(IntervalSet.span(start, end))

    def expand(self, before, after):
        """Every interval widened by `before` on the left and `after` on the right (both >= 0)."""
        if before < 0 or after < 0:
            raise ValueError("expand() takes non-negative widths")
        return IntervalSet((start - before, end + after) for start, end in self._items)

    def shift(self, delta):
        return IntervalSet._canonical([(start + delta, end + delta) for start, end in self._items])

    def longer_than(self, length):
        """Only the intervals of at least `length` integers."""
        return IntervalSet._canonical([(start, end) for start, end in self._items if end - start >= length])
