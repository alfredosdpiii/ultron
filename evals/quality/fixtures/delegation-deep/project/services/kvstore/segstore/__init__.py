"""segstore: a log-structured key-value store over in-memory segments."""
from .compaction import CompactionStats
from .record import HEADER_SIZE, CorruptRecord, Record, TornRecord, decode, encode
from .recovery import CorruptSegment, recover
from .store import Store

__all__ = [
    "HEADER_SIZE",
    "CompactionStats",
    "CorruptRecord",
    "CorruptSegment",
    "Record",
    "Store",
    "TornRecord",
    "decode",
    "encode",
    "recover",
]
