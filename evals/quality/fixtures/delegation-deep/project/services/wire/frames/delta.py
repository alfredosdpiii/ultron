"""Packed integer series: a count, then each value as the zigzag varint of its difference from the previous one."""
from .errors import DecodeError, EncodeError, TruncatedError
from .varint import SINT64_MAX, SINT64_MIN, decode_uvarint, encode_uvarint, unzigzag, zigzag


def _check_series(values):
    if not isinstance(values, (list, tuple)):
        raise EncodeError(f"a packed series must be a list or tuple, not {type(values).__name__}")
    for value in values:
        if isinstance(value, bool) or not isinstance(value, int):
            raise EncodeError(f"packed series values must be ints, not {type(value).__name__}")
        if not SINT64_MIN <= value <= SINT64_MAX:
            raise EncodeError(f"packed series value out of the 64-bit range: {value}")


def encode_deltas(values):
    """Encode a series of signed 64-bit integers. The first value is stored as its difference from zero."""
    _check_series(values)
    out = bytearray(encode_uvarint(len(values)))
    previous = 0
    for value in values:
        out += encode_uvarint(zigzag(value - previous))
        previous = value
    return bytes(out)


def decode_deltas(data, pos=0, end=None):
    """Decode a series from data[pos:end]; the series must use exactly those bytes. Returns a list."""
    end = len(data) if end is None else end
    count, index = decode_uvarint(data, pos)
    if index > end:
        raise TruncatedError("packed series count runs past its end")
    if count > end - index:
        raise TruncatedError(f"packed series declares {count} values in {end - index} bytes")
    values = []
    previous = 0
    for _ in range(count):
        raw, index = decode_uvarint(data, index)
        if index > end:
            raise TruncatedError("packed series value runs past its end")
        value = previous + unzigzag(raw)
        if not SINT64_MIN <= value <= SINT64_MAX:
            raise DecodeError(f"packed series value out of the 64-bit range: {value}")
        values.append(value)
        previous = value
    if index != end:
        raise DecodeError(f"{end - index} bytes left after a packed series of {count} values")
    return values
