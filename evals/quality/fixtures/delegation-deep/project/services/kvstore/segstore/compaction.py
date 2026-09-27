"""Compaction: rewrite the closed segments so they hold only records that are still live."""
from dataclasses import dataclass

from .record import encode


@dataclass
class CompactionStats:
    segments_before: int
    segments_after: int
    bytes_before: int
    bytes_after: int
    records_kept: int
    records_dropped: int


def _live_records(closed, index):
    """The closed segments' records that are still their key's newest, in write order, with their old locations."""
    live = []
    for segment in closed:
        for offset, record in segment.records():
            if not record.tombstone and index.get(record.key) == (segment.id, offset):
                live.append(record)
    return live


def compact(table, index):
    """Compact every closed segment of `table` in place and repoint `index` at the rewritten records.

    A record survives when it is its key's newest record overall (the one the index points at). Survivors keep their
    relative order and are packed into the closed segments from the oldest one on, so ids keep increasing in write
    order; closed segments left without records are dropped.
    """
    closed = table.closed()
    if not closed:
        return CompactionStats(0, 0, 0, 0, 0, 0)
    bytes_before = sum(segment.size for segment in closed)
    total_records = sum(1 for segment in closed for _ in segment.records())
    survivors = _live_records(closed, index)

    targets = iter(closed)
    used = []
    moves = []
    for record in survivors:
        raw = encode(record.key, record.value)
        if not used or not used[-1].fits(len(raw)):
            if used:
                used[-1].close()
            segment = next(targets)
            segment.rewrite()
            used.append(segment)
        moves.append((record.key, used[-1].id, used[-1].append(raw)))
    for segment in targets:
        segment.rewrite()

    table.replace_closed(used)
    for key, segment_id, offset in moves:
        index.set(key, segment_id, offset)
    return CompactionStats(
        segments_before=len(closed),
        segments_after=len(used),
        bytes_before=bytes_before,
        bytes_after=sum(segment.size for segment in used),
        records_kept=len(survivors),
        records_dropped=total_records - len(survivors),
    )
