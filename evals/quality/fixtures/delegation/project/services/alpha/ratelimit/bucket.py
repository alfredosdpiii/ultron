"""A token bucket over integer ticks."""
from .clock import advance


class TokenBucket:
    def __init__(self, capacity, refill_per_tick, now=0):
        if not isinstance(capacity, int) or capacity <= 0:
            raise ValueError("capacity must be a positive integer")
        if not isinstance(refill_per_tick, int) or refill_per_tick <= 0:
            raise ValueError("refill_per_tick must be a positive integer")
        self.capacity = capacity
        self.rate = refill_per_tick
        self.tokens = capacity
        self.last = now

    def check_amount(self, n):
        if n < 1 or n > self.capacity:
            raise ValueError(f"n must be between 1 and {self.capacity}")

    def refill(self, now):
        elapsed, self.last = advance(self.last, now)
        self.tokens = self.tokens + elapsed * self.rate
        return self.tokens

    def try_acquire(self, now, n=1):
        self.check_amount(n)
        self.refill(now)
        if self.tokens >= n:
            self.tokens -= n
            return True
        return False
