"""Variable-length integers: unsigned LEB128, and signed values through the zigzag mapping."""
from .errors import DecodeError, EncodeError, TruncatedError

MAX_VARINT_BYTES = 10
VARINT_LIMIT = 1 << (7 * MAX_VARINT_BYTES)

UINT64_MAX = (1 << 64) - 1
SINT64_MIN = -(1 << 63)
SINT64_MAX = (1 << 63) - 1


def _require_int(value, what):
    if isinstance(value, bool) or not isinstance(value, int):
        raise EncodeError(f"{what} must be an int, not {type(value).__name__}")


def encode_uvarint(value):
    """LEB128: seven bits per byte, least significant group first, high bit set on every byte but the last."""
    _require_int(value, "varint")
    if value < 0 or value >= VARINT_LIMIT:
        raise EncodeError(f"varint out of range: {value}")
    out = bytearray()
    while value > 0x7F:
        out.append((value & 0x7F) | 0x80)
        value >>= 7
    out.append(value)
    return bytes(out)


def decode_uvarint(data, pos=0):
    """Decode one varint starting at `pos`; returns (value, position after it)."""
    result = 0
    shift = 0
    index = pos
    while True:
        if index >= len(data):
            raise TruncatedError("varint runs past the end of the data")
        byte = data[index]
        index += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            break
        shift += 7
        if shift >= 7 * MAX_VARINT_BYTES:
            raise DecodeError(f"varint longer than {MAX_VARINT_BYTES} bytes")
    if byte == 0 and index - pos > 1:
        raise DecodeError("varint is not minimally encoded")
    return result, index


def uvarint_size(value):
    """Number of bytes encode_uvarint(value) produces."""
    size = 1
    while value > 0x7F:
        value >>= 7
        size += 1
    return size


def zigzag(value):
    """Map a signed integer onto the unsigned ones: 0, -1, 1, -2, 2, ... become 0, 1, 2, 3, 4, ..."""
    return (value << 1) ^ (value >> 63)


def unzigzag(value):
    """Inverse of zigzag."""
    return (value >> 1) ^ -(value & 1)


def encode_svarint(value):
    """A signed 64-bit integer, zigzag-mapped, as a varint."""
    _require_int(value, "signed varint")
    if not SINT64_MIN <= value <= SINT64_MAX:
        raise EncodeError(f"signed varint out of the 64-bit range: {value}")
    return encode_uvarint(zigzag(value))


def decode_svarint(data, pos=0):
    """Decode one signed 64-bit varint; returns (value, position after it)."""
    raw, end = decode_uvarint(data, pos)
    value = unzigzag(raw)
    if not SINT64_MIN <= value <= SINT64_MAX:
        raise DecodeError(f"signed varint out of the 64-bit range: {value}")
    return value, end
