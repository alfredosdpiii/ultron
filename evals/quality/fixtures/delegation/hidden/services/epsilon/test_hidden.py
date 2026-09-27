"""Hidden checks for services/epsilon (LRU cache with expiry): SPEC cases plus randomized comparison with a reference."""
import random
import unittest

from ttlcache import TTLCache


class RefCache:
    def __init__(self, capacity, ttl):
        self.capacity, self.ttl = capacity, ttl
        self.entries = []  # [key, value, expires_at], least recently used first
        self.hits = self.misses = self.evictions = 0

    def find(self, key):
        return next((e for e in self.entries if e[0] == key), None)

    def purge(self, now):
        self.entries = [e for e in self.entries if now < e[2]]

    def put(self, key, value, now):
        entry = self.find(key)
        if entry is not None:
            self.entries.remove(entry)
        elif len(self.entries) >= self.capacity:
            self.purge(now)
            if len(self.entries) >= self.capacity:
                self.entries.pop(0)
                self.evictions += 1
        self.entries.append([key, value, now + self.ttl])

    def get(self, key, now, default=None):
        entry = self.find(key)
        if entry is None or now >= entry[2]:
            if entry is not None:
                self.entries.remove(entry)
            self.misses += 1
            return default
        self.entries.remove(entry)
        self.entries.append(entry)
        self.hits += 1
        return entry[1]

    def delete(self, key):
        entry = self.find(key)
        if entry is not None:
            self.entries.remove(entry)
        return entry is not None

    def keys(self, now):
        self.purge(now)
        return [e[0] for e in self.entries]

    def stats(self):
        return {"hits": self.hits, "misses": self.misses, "evictions": self.evictions}


class EpsilonHidden(unittest.TestCase):
    def test_update_refreshes_recency_and_expiry(self):
        cache = TTLCache(2, 10)
        cache.put("a", 1, 0)
        cache.put("b", 2, 1)
        cache.put("a", 3, 2)
        cache.put("c", 4, 3)
        self.assertEqual(cache.keys(3), ["a", "c"])
        self.assertEqual(cache.get("a", 11), 3)
        self.assertIsNone(cache.get("a", 12))

    def test_expired_entries_make_room_before_eviction(self):
        cache = TTLCache(2, 5)
        cache.put("a", 1, 0)
        cache.put("b", 2, 3)
        cache.put("c", 3, 5)  # "a" expired at 5: no eviction needed
        self.assertEqual(cache.keys(5), ["b", "c"])
        self.assertEqual(cache.stats()["evictions"], 0)
        cache.put("d", 4, 6)  # nothing expired: "b" is evicted
        self.assertEqual(cache.keys(6), ["c", "d"])
        self.assertEqual(cache.stats()["evictions"], 1)

    def test_read_does_not_extend(self):
        cache = TTLCache(3, 4)
        cache.put("a", 1, 0)
        self.assertEqual(cache.get("a", 3), 1)
        self.assertEqual(cache.get("a", 4, "gone"), "gone")
        self.assertFalse(cache.delete("a"))

    def test_bad_arguments(self):
        for capacity, ttl in [(0, 1), (1, 0)]:
            with self.assertRaises(ValueError):
                TTLCache(capacity, ttl)

    def test_random_against_reference(self):
        for seed in range(600):
            rnd = random.Random(seed)
            capacity, ttl = rnd.randint(1, 5), rnd.randint(1, 8)
            cache, ref = TTLCache(capacity, ttl), RefCache(capacity, ttl)
            now = 0
            log = []
            for step in range(80):
                now += rnd.choice([0, 0, 1, 1, 2, 3])
                key = rnd.choice("abcdefg")
                op = rnd.random()
                if op < 0.45:
                    got, want = cache.put(key, step, now), ref.put(key, step, now)
                    log.append(("put", key, now))
                elif op < 0.85:
                    got, want = cache.get(key, now, "miss"), ref.get(key, now, "miss")
                    log.append(("get", key, now, want))
                elif op < 0.92:
                    stored = ref.find(key)
                    got, want = cache.delete(key), ref.delete(key)
                    log.append(("delete", key, want))
                    if stored is not None and now >= stored[2]:
                        got = want  # an expired entry may already have been removed: either answer is right
                else:
                    got, want = (cache.keys(now), cache.size(now)), (ref.keys(now), len(ref.keys(now)))
                    log.append(("keys", now, want))
                context = f"seed {seed}, capacity {capacity}, ttl {ttl}, last calls {log[-5:]}"
                self.assertEqual(got, want, context)
                self.assertEqual(cache.stats(), ref.stats(), context)


if __name__ == "__main__":
    unittest.main()
