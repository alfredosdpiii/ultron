"""The store: puts and deletes are appended to the active segment; reads go through the index."""
from .compaction import compact
from .index import Index, Location
from .record import encode
from .segment import SegmentTable

_DECODED_LIMIT = 1024


class Store:
    """A log-structured key-value store. Keys are non-empty strings, values are bytes."""

    def __init__(self, segment_limit=4096, *, _table=None, _index=None):
        if segment_limit <= 0:
            raise ValueError("segment_limit must be positive")
        self.segment_limit = segment_limit
        self._table = _table if _table is not None else SegmentTable(segment_limit)
        self._index = _index if _index is not None else Index()
        self._decoded = {}

    def __len__(self):
        return len(self._index)

    def __contains__(self, key):
        return key in self._index

    def _write(self, raw):
        if len(raw) > self.segment_limit:
            raise ValueError(f"record of {len(raw)} bytes does not fit in a segment of {self.segment_limit}")
        segment = self._table.active
        if not segment.fits(len(raw)):
            segment = self._table.roll()
        return Location(segment.id, segment.append(raw))

    def put(self, key, value):
        """Store `value` under `key`, replacing any earlier value."""
        location = self._write(encode(key, value))
        self._index.set(key, location.segment, location.offset)

    def get(self, key, default=None):
        """The value stored under `key`, or `default`."""
        location = self._index.get(key)
        if location is None:
            return default
        record = self._decoded.get(location)
        if record is None:
            record = self._table.get(location.segment).read(location.offset)
            if len(self._decoded) >= _DECODED_LIMIT:
                self._decoded.clear()
            self._decoded[location] = record
        return record.value

    def delete(self, key):
        """Remove `key`. Returns True if it was present; deleting a missing key writes nothing."""
        if key not in self._index:
            return False
        self._write(encode(key, tombstone=True))
        self._index.remove(key)
        return True

    def delete_prefix(self, prefix):
        """Delete every live key that begins with `prefix`; returns how many were deleted."""
        if not isinstance(prefix, str) or not prefix:
            raise ValueError("prefix must be a non-empty string")
        removed = 0
        for key in self._index.range(prefix=prefix):
            self.delete(key)
            removed += 1
        return removed

    def scan(self, start=None, end=None, prefix=None):
        """(key, value) pairs in key order with start <= key < end, restricted to keys beginning with `prefix`."""
        return [(key, self.get(key)) for key in self._index.range(start, end, prefix)]

    def keys(self):
        return [key for key, _ in self._index.items()]

    def compact(self):
        """Rewrite the closed segments without dead records. The active segment is left as it is."""
        return compact(self._table, self._index)

    def export(self):
        """Every segment as (segment_id, bytes), in the store's write order."""
        return [(segment.id, segment.to_bytes()) for segment in self._table.all()]

    def stats(self):
        segments = self._table.all()
        return {
            "segments": len(segments),
            "active_segment": self._table.active.id,
            "live_keys": len(self._index),
            "bytes": sum(segment.size for segment in segments),
        }
