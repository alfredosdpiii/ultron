"""Compaction: rewrite the closed segments so they hold only records that are still live."""
from dataclasses import dataclass

from .record import encode
from .segment import Segment


@dataclass
class CompactionStats:
    segments_before: int
    segments_after: int
    bytes_before: int
    bytes_after: int
    records_kept: int
    records_dropped: int


def _newest_closed_records(closed):
    """For every key in the closed segments, its newest closed record: key -> (position, offset, record)."""
    newest = {}
    for position in range(len(closed) - 1, -1, -1):
        for offset, record in closed[position].records():
            newest.setdefault(record.key, (position, offset, record))
    return newest


def compact(table, index):
    """Compact every closed segment of `table` and repoint `index` at the rewritten records.

    A key's record survives when it is the key's newest record overall; a key whose newest record is in the active
    segment, or is a tombstone, keeps nothing in the closed segments. Surviving records keep their relative order.
    """
    closed = table.closed()
    active_id = table.active.id
    bytes_before = sum(segment.size for segment in closed)
    total_records = sum(1 for segment in closed for _ in segment.records())
    if not closed:
        return CompactionStats(0, 0, 0, 0, 0, 0)

    survivors = []
    for key, (position, offset, record) in _newest_closed_records(closed).items():
        if record.tombstone:
            continue
        location = index.get(key)
        if location is None or location.segment == active_id:
            continue
        survivors.append((position, offset, record))
    survivors.sort(key=lambda item: (item[0], item[1]))

    ids = table.compaction_ids()
    rewritten = []
    moves = []
    current = None
    for _, _, record in survivors:
        raw = encode(record.key, record.value)
        if current is None or not current.fits(len(raw)):
            if current is not None:
                current.close()
            current = Segment(next(ids), table.limit)
            rewritten.append(current)
        moves.append((record.key, current.id, current.append(raw)))

    table.replace_closed(rewritten)
    for key, segment_id, offset in moves:
        index.set(key, segment_id, offset)
    return CompactionStats(
        segments_before=len(closed),
        segments_after=len(rewritten),
        bytes_before=bytes_before,
        bytes_after=sum(segment.size for segment in rewritten),
        records_kept=len(survivors),
        records_dropped=total_records - len(survivors),
    )
