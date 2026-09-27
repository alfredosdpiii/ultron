# alpha: per-key rate limiter

A token-bucket rate limiter over integer ticks. Callers pass the current tick (`now`, an integer) into every call;
nothing reads a real clock.

## TokenBucket(capacity, refill_per_tick, now=0)

- `capacity` and `refill_per_tick` are positive integers; anything else raises `ValueError`.
- A new bucket is full (`tokens == capacity`) and has seen tick `now`.
- `refill(now)` adds `refill_per_tick` tokens for every tick elapsed since the last tick the bucket saw, never
  holding more than `capacity`, and returns the token count.
- Time never goes backwards for a bucket: a `now` earlier than the last tick it saw counts as that last tick
  (no refill, and the bucket keeps its last tick, so a later call is not refilled twice for the same ticks).
- `try_acquire(now, n=1)` refills at `now`, then takes `n` tokens and returns `True` if at least `n` are there;
  otherwise it takes nothing and returns `False`. `n` must satisfy `1 <= n <= capacity`, else `ValueError`.

## Limiter(capacity, refill_per_tick)

- Keeps one bucket per key, created full on the key's first use at that call's `now`.
- `allow(key, now, n=1)` is `try_acquire` on the key's bucket.
- `retry_after(key, now, n=1)` returns how many ticks from `now` the caller must wait until `allow(key, ..., n)`
  would succeed, assuming no other calls: `0` if it would succeed at `now`, else the smallest whole number of ticks
  whose refill covers the shortfall (round up). If `now` is earlier than the bucket's last tick, no refill happens
  before that last tick, so the wait also includes the ticks from `now` up to it. It refills the bucket at `now`
  like any call, but takes no tokens.
  `n` must satisfy `1 <= n <= capacity`, else `ValueError`.
