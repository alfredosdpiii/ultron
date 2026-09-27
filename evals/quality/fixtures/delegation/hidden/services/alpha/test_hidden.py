"""Hidden checks for services/alpha (rate limiter): SPEC cases plus randomized comparison with a reference."""
import random
import unittest

from ratelimit import Limiter, TokenBucket


class RefBucket:
    def __init__(self, capacity, rate, now=0):
        self.capacity, self.rate, self.tokens, self.last = capacity, rate, capacity, now

    def refill(self, now):
        if now > self.last:
            self.tokens = min(self.capacity, self.tokens + (now - self.last) * self.rate)
            self.last = now
        return self.tokens


class RefLimiter:
    def __init__(self, capacity, rate):
        self.capacity, self.rate, self.buckets = capacity, rate, {}

    def bucket(self, key, now):
        if key not in self.buckets:
            self.buckets[key] = RefBucket(self.capacity, self.rate, now)
        return self.buckets[key]

    def allow(self, key, now, n):
        bucket = self.bucket(key, now)
        if not 1 <= n <= self.capacity:
            return "ValueError"
        if bucket.refill(now) >= n:
            bucket.tokens -= n
            return True
        return False

    def retry_after(self, key, now, n):
        bucket = self.bucket(key, now)
        if not 1 <= n <= self.capacity:
            return "ValueError"
        tokens = bucket.refill(now)
        return 0 if tokens >= n else max(0, bucket.last - now) + -(-(n - tokens) // self.rate)


def outcome(call):
    try:
        return call()
    except ValueError:
        return "ValueError"


class AlphaHidden(unittest.TestCase):
    def test_capacity_cap(self):
        bucket = TokenBucket(3, 2)
        bucket.try_acquire(0, 3)
        self.assertEqual(bucket.refill(1), 2)
        self.assertEqual(bucket.refill(2), 3)
        self.assertEqual(bucket.refill(50), 3)

    def test_time_never_goes_backwards(self):
        bucket = TokenBucket(10, 1)
        self.assertTrue(bucket.try_acquire(10, 10))
        self.assertEqual(bucket.refill(5), 0)
        self.assertFalse(bucket.try_acquire(7))
        self.assertEqual(bucket.refill(12), 2)
        self.assertEqual(bucket.last, 12)

    def test_failed_acquire_takes_nothing(self):
        bucket = TokenBucket(4, 1)
        bucket.try_acquire(0, 3)
        self.assertFalse(bucket.try_acquire(0, 2))
        self.assertTrue(bucket.try_acquire(0, 1))

    def test_bad_amounts(self):
        limiter = Limiter(3, 1)
        for n in (0, -1, 4):
            with self.assertRaises(ValueError):
                limiter.allow("k", 0, n)
            with self.assertRaises(ValueError):
                limiter.retry_after("k", 0, n)
        with self.assertRaises(ValueError):
            TokenBucket(0, 1)

    def test_retry_after(self):
        limiter = Limiter(10, 3)
        self.assertEqual(limiter.retry_after("a", 0, 10), 0)
        limiter.allow("a", 0, 10)
        self.assertEqual(limiter.retry_after("a", 0, 1), 1)
        self.assertEqual(limiter.retry_after("a", 0, 3), 1)
        self.assertEqual(limiter.retry_after("a", 0, 4), 2)
        self.assertEqual(limiter.retry_after("a", 0, 10), 4)
        self.assertEqual(limiter.retry_after("a", 1, 4), 1)
        self.assertTrue(limiter.allow("a", 2, 4))
        # Before the bucket's last tick nothing refills: the wait runs from `now` to that tick, then the refill.
        self.assertTrue(limiter.allow("a", 2, 2))
        self.assertEqual(limiter.retry_after("a", 0, 3), 2 + 1)

    def test_random_against_reference(self):
        for seed in range(400):
            rnd = random.Random(seed)
            capacity, rate = rnd.randint(1, 12), rnd.randint(1, 4)
            limiter, ref = Limiter(capacity, rate), RefLimiter(capacity, rate)
            now = rnd.randint(0, 5)
            trace = []
            for _ in range(80):
                now = max(0, now + rnd.choice([-3, -1, 0, 0, 0, 1, 1, 2, 3, 5]))
                key = rnd.choice("abc")
                n = rnd.randint(0, capacity + 1) if rnd.random() < 0.1 else rnd.randint(1, capacity)
                if rnd.random() < 0.7:
                    got, want = outcome(lambda: limiter.allow(key, now, n)), ref.allow(key, now, n)
                    trace.append(("allow", key, now, n, want))
                else:
                    got, want = outcome(lambda: limiter.retry_after(key, now, n)), ref.retry_after(key, now, n)
                    trace.append(("retry_after", key, now, n, want))
                self.assertEqual(got, want, f"seed {seed}, capacity {capacity}, rate {rate}, last calls {trace[-4:]}")


if __name__ == "__main__":
    unittest.main()
