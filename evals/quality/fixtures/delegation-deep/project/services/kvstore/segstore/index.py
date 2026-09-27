"""In-memory index: key -> location of the key's newest live record, plus the keys in sorted order for scans."""
import bisect
from typing import NamedTuple


class Location(NamedTuple):
    segment: int
    offset: int


class KeyRange:
    """A live, ordered view over a sorted key list, like the views of a dict."""

    def __init__(self, keys, start, end, prefix):
        self._keys = keys
        self.start = start
        self.end = end
        self.prefix = prefix or None

    def _first(self):
        low = 0
        if self.start is not None:
            low = bisect.bisect_left(self._keys, self.start)
        if self.prefix is not None:
            low = max(low, bisect.bisect_left(self._keys, self.prefix))
        return low

    def __iter__(self):
        position = self._first()
        while position < len(self._keys):
            key = self._keys[position]
            if self.end is not None and key >= self.end:
                return
            if self.prefix is not None and not key.startswith(self.prefix):
                return
            yield key
            position += 1

    def __len__(self):
        return sum(1 for _ in self)

    def __repr__(self):
        return f"KeyRange(start={self.start!r}, end={self.end!r}, prefix={self.prefix!r})"


class Index:
    def __init__(self):
        self._locations = {}
        self._keys = []

    def __len__(self):
        return len(self._locations)

    def __contains__(self, key):
        return key in self._locations

    def get(self, key):
        """The location of `key`'s live record, or None."""
        return self._locations.get(key)

    def set(self, key, segment, offset):
        if key not in self._locations:
            bisect.insort(self._keys, key)
        self._locations[key] = Location(segment, offset)

    def remove(self, key):
        """Forget `key`; returns whether it was there."""
        if self._locations.pop(key, None) is None:
            return False
        position = bisect.bisect_left(self._keys, key)
        del self._keys[position]
        return True

    def range(self, start=None, end=None, prefix=None):
        """A view of the keys, in order, with start <= key < end (either bound optional) that begin with `prefix`."""
        return KeyRange(self._keys, start, end, prefix)

    def items(self):
        for key in self._keys:
            yield key, self._locations[key]
