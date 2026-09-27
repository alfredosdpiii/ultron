# kvstore: log-structured key-value store

`segstore` keeps key-value pairs in append-only, in-memory byte segments. Every write appends a record to the
active segment; an in-memory index maps each live key to its newest record. Keys are non-empty `str`, values are
`bytes`. Standard library only.

## Records (`segstore.encode`, `segstore.decode`)

A record is an 11-byte header (`HEADER_SIZE`) followed by the key (UTF-8) and the value:

| field | size | meaning |
| --- | --- | --- |
| crc32 | 4 bytes, big-endian | `zlib.crc32` of everything after it (flags, lengths, key, value) |
| flags | 1 byte | bit 0 set for a tombstone (a delete); a tombstone has an empty value |
| klen | 2 bytes, big-endian | key length in bytes |
| vlen | 4 bytes, big-endian | value length in bytes |

- `encode(key, value=b"", tombstone=False)` returns the record's bytes. An empty or non-`str` key raises
  `ValueError`, a non-bytes value `TypeError`.
- `decode(buffer, offset=0)` returns `(Record(key, value, tombstone), offset_after_record)`. It raises
  `TornRecord` when the buffer ends inside the record (header or body) and `CorruptRecord` when the checksum
  does not match.

## Store(segment_limit=4096)

- Segments have ids (integers) and hold at most `segment_limit` bytes. A new store has one empty active segment,
  id 0.
- A write appends one record to the active segment. If the record does not fit (the segment's size plus the
  record's size would exceed `segment_limit`), the active segment is closed first and a new active segment with the
  next id (one more than the highest id the store has used) is opened; the record is written at its offset 0.
  A record larger than `segment_limit` raises `ValueError` and writes nothing.
- `put(key, value)` stores the value (replacing any earlier one).
- `get(key, default=None)` returns the newest value, or `default` when the key is missing or deleted.
- `delete(key)` removes a present key by appending a tombstone and returns `True`; deleting a missing key writes
  nothing and returns `False`.
- `delete_prefix(prefix)` deletes (as `delete` does) every live key that begins with `prefix` and returns how many
  it deleted. An empty or non-`str` prefix raises `ValueError`.
- `scan(start=None, end=None, prefix=None)` returns a list of `(key, value)` pairs sorted by key (Python string
  order) with `start <= key < end` (either bound may be `None`) and, when `prefix` is non-empty, only keys that
  begin with `prefix`.
- `keys()` returns the live keys in sorted order; `len(store)` their number; `key in store` works.
- `export()` returns every segment as `(segment_id, data)` (`data` is bytes-like), closed segments first and the
  active one last.
- `stats()` returns `{"segments", "active_segment", "live_keys", "bytes"}` (segment count, the active segment's id,
  live key count, total bytes over all segments).

## Compaction: `store.compact()`

Rewrites all closed segments (never the active one) so they hold only live records:

- A record survives only if it is its key's newest record in the whole store and is not a tombstone. So a key
  deleted or rewritten later (in a closed or the active segment) keeps nothing in the closed segments, and
  tombstones in closed segments are dropped (nothing older than them remains).
- Survivors keep their relative order and are packed into closed segments with the same rule as writes (a
  record that does not fit starts the next segment). Compaction never adds segments or bytes.
- Segment ids always increase in write order, compaction included, so every closed segment's id is lower than
  the active segment's. Reads return the same values before and after a compaction; later writes continue as
  before.
- Returns a `CompactionStats` (segments and bytes before and after, records kept and dropped).

## Recovery: `recover(segments, segment_limit)`

Rebuilds a store from exported segments, given as `(segment_id, bytes)` pairs in any order:

- Segments are replayed in ascending id order, records in each segment in order: a put makes its record the
  key's newest, a tombstone removes the key.
- The segment with the highest id was being written when the store stopped. Its bytes are replayed up to the
  first record that is torn or corrupt; that record and everything after it are cut off. That segment becomes the
  new active segment (holding only its undamaged prefix, so later writes append after it), all others are closed,
  and new segments continue from the highest id plus one.
- A torn or corrupt record in any other segment raises `CorruptSegment`. Duplicate ids raise `ValueError`.
- No segments at all gives an empty store. The recovered store answers every read exactly as the original store
  did at export time (minus whatever was cut off from the last segment).

## Examples

    store = Store(segment_limit=30)
    store.put("a", b"1"); store.put("b", b"2")   # 13 bytes each, both in segment 0
    store.put("a", b"3")                          # does not fit: segment 0 closes, written at offset 0 of segment 1
    store.get("a")          # b"3"
    store.delete("b")       # True (a tombstone in segment 1); store.delete("b") again is False
    store.scan()            # [("a", b"3")]
    store.compact()         # segment 0 held only dead records: nothing survives, it is dropped
    recover(store.export(), 30).scan()   # [("a", b"3")]
