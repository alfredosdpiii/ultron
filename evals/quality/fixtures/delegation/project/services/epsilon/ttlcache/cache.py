"""The public cache."""
from .entry import Entry
from .store import Store


class TTLCache:
    def __init__(self, capacity, ttl):
        if capacity < 1 or ttl < 1:
            raise ValueError("capacity and ttl must be at least 1")
        self.capacity = capacity
        self.ttl = ttl
        self._store = Store()
        self._hits = self._misses = self._evictions = 0

    def get(self, key, now, default=None):
        entry = self._store.get(key)
        if entry is None or entry.expired(now):
            if entry is not None:
                self._store.delete(key)
            self._misses += 1
            return default
        self._store.touch(key)
        self._hits += 1
        return entry.value

    def put(self, key, value, now):
        if self._store.get(key) is None and len(self._store) >= self.capacity:
            self._evict()
        self._store.set(key, Entry(value, now + self.ttl))

    def delete(self, key):
        return self._store.delete(key)

    def _purge(self, now):
        for key in self._store.keys():
            if self._store.get(key).expired(now):
                self._store.delete(key)

    def _evict(self):
        self._store.delete(self._store.oldest())
        self._evictions += 1

    def keys(self, now):
        self._purge(now)
        return self._store.keys()

    def size(self, now):
        return len(self.keys(now))

    def stats(self):
        return {"hits": self._hits, "misses": self._misses, "evictions": self._evictions}
