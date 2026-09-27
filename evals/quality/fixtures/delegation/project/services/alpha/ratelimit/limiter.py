"""Per-key rate limiting on top of TokenBucket."""
from .bucket import TokenBucket


class Limiter:
    def __init__(self, capacity, refill_per_tick):
        self.capacity = capacity
        self.rate = refill_per_tick
        self._buckets = {}

    def _bucket(self, key, now):
        bucket = self._buckets.get(key)
        if bucket is None:
            bucket = self._buckets[key] = TokenBucket(self.capacity, self.rate, now)
        return bucket

    def allow(self, key, now, n=1):
        return self._bucket(key, now).try_acquire(now, n)

    def retry_after(self, key, now, n=1):
        """Ticks to wait from `now` until allow(key, ..., n) would succeed; 0 if it would now. Takes no tokens."""
        bucket = self._bucket(key, now)
        bucket.check_amount(n)
        tokens = bucket.refill(now)
        if tokens >= n:
            return 0
        return (n - tokens) // self.rate
