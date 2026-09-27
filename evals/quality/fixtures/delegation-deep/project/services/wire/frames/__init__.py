"""frames: varints, packed integer series, typed records and a CRC-checked framed container."""
from .container import HEADER, MAGIC, MAX_PAYLOAD, VERSION, Frame, decode_stream, encode_frame, encode_stream, frame_crc
from .delta import decode_deltas, encode_deltas
from .errors import (
    BadHeader,
    CorruptFrame,
    DecodeError,
    EncodeError,
    StreamError,
    TruncatedError,
    TruncatedStream,
    WireError,
)
from .record import Schema, decode_record, encode_record
from .stream import StreamDecoder
from .varint import (
    SINT64_MAX,
    SINT64_MIN,
    UINT64_MAX,
    decode_svarint,
    decode_uvarint,
    encode_svarint,
    encode_uvarint,
    unzigzag,
    zigzag,
)

__all__ = [
    "BadHeader",
    "CorruptFrame",
    "DecodeError",
    "EncodeError",
    "Frame",
    "HEADER",
    "MAGIC",
    "MAX_PAYLOAD",
    "SINT64_MAX",
    "SINT64_MIN",
    "Schema",
    "StreamDecoder",
    "StreamError",
    "TruncatedError",
    "TruncatedStream",
    "UINT64_MAX",
    "VERSION",
    "WireError",
    "decode_deltas",
    "decode_record",
    "decode_stream",
    "decode_svarint",
    "decode_uvarint",
    "encode_deltas",
    "encode_frame",
    "encode_record",
    "encode_stream",
    "encode_svarint",
    "encode_uvarint",
    "frame_crc",
    "unzigzag",
    "zigzag",
]
