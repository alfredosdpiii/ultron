import unittest

from ttlcache import TTLCache


class QuickTest(unittest.TestCase):
    def test_expiry_boundary(self):
        cache = TTLCache(4, 10)
        cache.put("a", 1, 0)
        self.assertEqual(cache.get("a", 9), 1)
        self.assertIsNone(cache.get("a", 10))
        self.assertEqual(cache.stats(), {"hits": 1, "misses": 1, "evictions": 0})

    def test_lru_eviction(self):
        cache = TTLCache(2, 100)
        cache.put("a", 1, 0)
        cache.put("b", 2, 1)
        cache.get("a", 2)
        cache.put("c", 3, 3)
        self.assertEqual(cache.keys(4), ["a", "c"])


if __name__ == "__main__":
    unittest.main()
