"""Entries in recency order, least recently used first."""
from collections import OrderedDict


class Store:
    def __init__(self):
        self._entries = OrderedDict()

    def get(self, key):
        return self._entries.get(key)

    def touch(self, key):
        self._entries.move_to_end(key)

    def set(self, key, entry):
        self._entries[key] = entry

    def delete(self, key):
        return self._entries.pop(key, None) is not None

    def oldest(self):
        return next(iter(self._entries), None)

    def keys(self):
        return list(self._entries)

    def __len__(self):
        return len(self._entries)
