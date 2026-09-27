"""The framed container: a header, then frames of (type, length, payload, CRC32)."""
import zlib
from collections import namedtuple

from .errors import EncodeError
from .varint import encode_uvarint

MAGIC = b"WIRE"
VERSION = 1
HEADER = MAGIC + bytes([VERSION])
MAX_PAYLOAD = 1 << 16

Frame = namedtuple("Frame", ["type", "payload"])


def frame_crc(frame_type, payload):
    """CRC32 of the type byte followed by the payload."""
    return zlib.crc32(bytes([frame_type]) + bytes(payload)) & 0xFFFFFFFF


def encode_frame(frame_type, payload):
    """One frame: type byte (1-255), varint payload length, payload, CRC32 big-endian."""
    if isinstance(frame_type, bool) or not isinstance(frame_type, int) or not 1 <= frame_type <= 255:
        raise EncodeError(f"frame type must be an int from 1 to 255: {frame_type!r}")
    if not isinstance(payload, (bytes, bytearray)):
        raise EncodeError(f"frame payload must be bytes, not {type(payload).__name__}")
    if len(payload) > MAX_PAYLOAD:
        raise EncodeError(f"frame payload of {len(payload)} bytes exceeds {MAX_PAYLOAD}")
    return (
        bytes([frame_type])
        + encode_uvarint(len(payload))
        + bytes(payload)
        + frame_crc(frame_type, payload).to_bytes(4, "big")
    )


def encode_stream(frames):
    """The header followed by every frame, in order. `frames` holds Frame tuples or (type, payload) pairs."""
    out = bytearray(HEADER)
    for frame_type, payload in frames:
        out += encode_frame(frame_type, payload)
    return bytes(out)


def decode_stream(data):
    """Decode a complete stream at once; frames with a bad CRC are dropped. Returns a list of Frame."""
    from .stream import StreamDecoder

    decoder = StreamDecoder()
    frames = decoder.feed(data)
    decoder.close()
    return frames
