"""Segments: append-only byte buffers with a size limit, and the table that orders them."""
from .record import decode


class SegmentFull(Exception):
    """A record was appended to a segment that has no room for it."""


class Segment:
    """One append-only segment. Offsets are byte positions from the start of the segment."""

    def __init__(self, segment_id, limit, data=b""):
        if limit <= 0:
            raise ValueError("segment limit must be positive")
        self.id = segment_id
        self.limit = limit
        self.closed = False
        self._data = bytearray(data)

    @property
    def size(self):
        return len(self._data)

    def fits(self, length):
        """Whether a record of `length` bytes can still be appended."""
        return not self.closed and self.size + length <= self.limit

    def append(self, raw):
        """Append one encoded record and return the offset it was written at."""
        if not self.fits(len(raw)):
            raise SegmentFull(f"segment {self.id} has {self.limit - self.size} bytes left, record needs {len(raw)}")
        offset = self.size
        self._data.extend(raw)
        return offset

    def read(self, offset):
        """The record at `offset`."""
        record, _ = decode(self._data, offset)
        return record

    def records(self):
        """Every record in write order, as (offset, record) pairs."""
        offset = 0
        while offset < self.size:
            record, following = decode(self._data, offset)
            yield offset, record
            offset = following

    def close(self):
        self.closed = True

    def to_bytes(self):
        return bytes(self._data)

    def __repr__(self):
        state = "closed" if self.closed else "open"
        return f"Segment(id={self.id}, size={self.size}/{self.limit}, {state})"


class SegmentTable:
    """The store's segments in write order: closed ones first, the single open (active) one last."""

    def __init__(self, limit, segments=None, next_id=None):
        self.limit = limit
        self._segments = list(segments or [])
        if not self._segments:
            self._segments.append(Segment(0, limit))
        self._next_id = next_id if next_id is not None else max(s.id for s in self._segments) + 1

    @property
    def active(self):
        return self._segments[-1]

    def closed(self):
        return self._segments[:-1]

    def all(self):
        return list(self._segments)

    def get(self, segment_id):
        for segment in self._segments:
            if segment.id == segment_id:
                return segment
        raise KeyError(f"no segment {segment_id}")

    def roll(self):
        """Close the active segment and open a new, empty one; returns the new active segment."""
        self.active.close()
        segment = Segment(self._next_id, self.limit)
        self._next_id += 1
        self._segments.append(segment)
        return segment

    def compaction_ids(self):
        """Ids for the segments a compaction writes, in the order it writes them."""
        start = self._next_id
        self._next_id += len(self._segments)
        return iter(range(start, self._next_id))

    def replace_closed(self, segments):
        """Swap every closed segment for `segments` (already closed), keeping the active one last."""
        for segment in segments:
            segment.close()
        self._segments = list(segments) + [self.active]

    def total_bytes(self):
        return sum(segment.size for segment in self._segments)
