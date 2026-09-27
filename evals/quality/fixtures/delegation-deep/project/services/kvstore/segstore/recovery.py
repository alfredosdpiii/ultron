"""Recovery: rebuild a store by replaying exported segments."""
from .index import Index
from .record import CorruptRecord, TornRecord, decode
from .segment import Segment, SegmentTable
from .store import Store


class CorruptSegment(Exception):
    """A segment other than the last one is damaged; recovery cannot tell what was lost."""


def _valid_prefix(data):
    """(records as (offset, record), length of the undamaged prefix) of one segment's bytes."""
    records = []
    offset = 0
    while offset < len(data):
        try:
            record, following = decode(data, offset)
        except (TornRecord, CorruptRecord):
            break
        records.append((offset, record))
        offset = following
    return records, offset


def recover(segments, segment_limit):
    """A store rebuilt from `segments`, an iterable of (segment_id, bytes) in any order.

    Segments are replayed in ascending id order. The segment with the highest id was being written when the store
    stopped: a torn or corrupt record there is cut off together with everything after it, and that segment becomes
    the active one. Damage in any other segment raises CorruptSegment.
    """
    ordered = sorted(segments, key=lambda item: item[0])
    if not ordered:
        return Store(segment_limit)
    ids = [segment_id for segment_id, _ in ordered]
    if len(set(ids)) != len(ids):
        raise ValueError("segment ids must be unique")

    index = Index()
    rebuilt = []
    for position, (segment_id, data) in enumerate(ordered):
        last = position == len(ordered) - 1
        records, valid = _valid_prefix(data)
        if valid != len(data) and not last:
            raise CorruptSegment(f"segment {segment_id} is damaged at byte {valid}")
        if len(data) > segment_limit:
            raise ValueError(f"segment {segment_id} is larger than the segment limit")
        segment = Segment(segment_id, segment_limit, data[:valid])
        for offset, record in records:
            if record.tombstone:
                index.remove(record.key)
            else:
                index.set(record.key, segment_id, offset)
        if not last:
            segment.close()
        rebuilt.append(segment)

    table = SegmentTable(segment_limit, rebuilt, next_id=ids[-1] + 1)
    return Store(segment_limit, _table=table, _index=index)
