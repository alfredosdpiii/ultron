import unittest

from ratelimit import Limiter, TokenBucket


class QuickTest(unittest.TestCase):
    def test_burst_then_refill(self):
        bucket = TokenBucket(5, 1)
        self.assertEqual([bucket.try_acquire(0) for _ in range(6)], [True] * 5 + [False])
        self.assertTrue(bucket.try_acquire(1))
        self.assertFalse(bucket.try_acquire(1))

    def test_idle_bucket_holds_at_most_capacity(self):
        bucket = TokenBucket(5, 1)
        bucket.try_acquire(0, 5)
        self.assertEqual(bucket.refill(100), 5)

    def test_retry_after_rounds_up(self):
        limiter = Limiter(4, 2)
        self.assertTrue(limiter.allow("k", 0, 4))
        self.assertEqual(limiter.retry_after("k", 0, 3), 2)
        self.assertTrue(limiter.allow("k", 2, 3))


if __name__ == "__main__":
    unittest.main()
