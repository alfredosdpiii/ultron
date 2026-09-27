# epsilon: LRU cache with expiry

A bounded key-value cache. Entries expire a fixed number of ticks after they were written, and when the cache is
full the least recently used entry makes room. Time is an integer tick (`now`) passed into every call.

## TTLCache(capacity, ttl)

`capacity >= 1` entries at most; every entry lives `ttl >= 1` ticks. Anything else raises `ValueError`.

- An entry written at tick `t` expires at tick `t + ttl`: it is still live at `t + ttl - 1` and already expired at
  `t + ttl` (and later).
- `put(key, value, now)` writes the entry with a fresh expiry and makes it the most recently used, whether the key
  was new or already present. Writing a new key when the cache holds `capacity` entries first removes every expired
  entry; only if the cache is still full is the least recently used entry evicted to make room.
- `get(key, now, default=None)` returns the value of a live entry and makes it the most recently used (a read does
  not extend its expiry). A missing or expired key returns `default`; an expired entry is removed.
- `delete(key)` removes the key and returns whether an entry for it was stored. It is `True` for a live entry and
  `False` for a key never written or already removed; for an expired entry it may be either, since expired entries
  may be removed at any time.
- `keys(now)` removes expired entries and returns the live keys, least recently used first. `size(now)` is
  `len(keys(now))`.
- `stats()` returns `{"hits": ..., "misses": ..., "evictions": ...}`: `get` calls that found a live entry, `get`
  calls that did not, and entries evicted to make room. Removing expired entries is not an eviction.
