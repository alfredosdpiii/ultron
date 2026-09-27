"""In-memory index: key -> location of the key's newest live record, plus the keys in sorted order for scans."""
import bisect
from typing import NamedTuple


class Location(NamedTuple):
    segment: int
    offset: int


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

    def keys(self, start=None, end=None, prefix=None):
        """Keys in order with start <= key < end (either bound optional) that begin with `prefix`."""
        low = 0
        if start is not None:
            low = bisect.bisect_left(self._keys, start)
        if prefix:
            low = max(low, bisect.bisect_left(self._keys, prefix))
        high = len(self._keys)
        if end is not None:
            high = bisect.bisect_left(self._keys, end)
        for position in range(low, high):
            key = self._keys[position]
            if prefix and not key.startswith(prefix):
                if key > prefix:
                    break
                continue
            yield key

    def items(self):
        for key in self._keys:
            yield key, self._locations[key]
