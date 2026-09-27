"""Record encoding.

A record is an 11-byte header followed by the key (UTF-8) and the value:

    crc32   4 bytes, big-endian, over everything after it (flags, lengths, key, value)
    flags   1 byte, bit 0 set for a tombstone
    klen    2 bytes, big-endian, key length in bytes
    vlen    4 bytes, big-endian, value length in bytes
"""
import struct
import zlib
from dataclasses import dataclass

HEADER = struct.Struct(">IBHI")
HEADER_SIZE = HEADER.size
FLAG_TOMBSTONE = 0x01
MAX_KEY_BYTES = 0xFFFF


class TornRecord(Exception):
    """The buffer ends before the record does."""


class CorruptRecord(Exception):
    """The record's checksum does not match its bytes."""


@dataclass(frozen=True)
class Record:
    key: str
    value: bytes
    tombstone: bool = False

    @property
    def size(self):
        return HEADER_SIZE + len(self.key.encode("utf-8")) + len(self.value)


def _checksum(flags, key_bytes, value):
    body = struct.pack(">BHI", flags, len(key_bytes), len(value)) + key_bytes + value
    return zlib.crc32(body) & 0xFFFFFFFF


def encode(key, value=b"", tombstone=False):
    """The bytes of one record. A tombstone carries no value."""
    if not isinstance(key, str) or not key:
        raise ValueError("key must be a non-empty string")
    if not isinstance(value, (bytes, bytearray)):
        raise TypeError("value must be bytes")
    key_bytes = key.encode("utf-8")
    if len(key_bytes) > MAX_KEY_BYTES:
        raise ValueError("key is too long")
    value = b"" if tombstone else bytes(value)
    flags = FLAG_TOMBSTONE if tombstone else 0
    crc = _checksum(flags, key_bytes, value)
    return HEADER.pack(crc, flags, len(key_bytes), len(value)) + key_bytes + value


def decode(buffer, offset=0):
    """Decode the record at `offset`: returns (record, offset of the next record).

    Raises TornRecord when the buffer ends inside the record and CorruptRecord when its checksum does not match.
    """
    end = len(buffer)
    if offset + HEADER_SIZE > end:
        raise TornRecord(f"header at {offset} runs past the end ({end})")
    crc, flags, key_len, value_len = HEADER.unpack_from(buffer, offset)
    start = offset + HEADER_SIZE
    stop = start + key_len + value_len
    if stop > end:
        raise TornRecord(f"record at {offset} runs past the end ({end})")
    key_bytes = bytes(buffer[start : start + key_len])
    value = bytes(buffer[start + key_len : stop])
    if _checksum(flags, key_bytes, value) != crc:
        raise CorruptRecord(f"checksum mismatch in record at {offset}")
    try:
        key = key_bytes.decode("utf-8")
    except UnicodeDecodeError as error:
        raise CorruptRecord(f"key of record at {offset} is not UTF-8") from error
    return Record(key, value, bool(flags & FLAG_TOMBSTONE)), stop
