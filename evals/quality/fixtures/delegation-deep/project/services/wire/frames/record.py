"""Records: typed fields, each written as a key varint (field id and wire type) followed by its value."""
from .delta import decode_deltas, encode_deltas
from .errors import DecodeError, EncodeError, TruncatedError
from .varint import (
    SINT64_MAX,
    SINT64_MIN,
    UINT64_MAX,
    decode_svarint,
    decode_uvarint,
    encode_svarint,
    encode_uvarint,
)

WIRE_VARINT = 0
WIRE_BYTES = 2

FIELD_TYPES = {"uint": WIRE_VARINT, "sint": WIRE_VARINT, "bytes": WIRE_BYTES, "str": WIRE_BYTES, "sints": WIRE_BYTES}
MAX_FIELD_ID = (1 << 29) - 1

SERIES_CACHE_SIZE = 256
_series_cache = {}


class Schema:
    """Field id -> type name. Ids run from 1 to 2**29 - 1; types are the keys of FIELD_TYPES."""

    def __init__(self, fields):
        if not isinstance(fields, dict):
            raise TypeError("a schema is a dict of field id -> type name")
        self.fields = {}
        for field_id, kind in fields.items():
            if isinstance(field_id, bool) or not isinstance(field_id, int) or not 1 <= field_id <= MAX_FIELD_ID:
                raise ValueError(f"invalid field id: {field_id!r}")
            if kind not in FIELD_TYPES:
                raise ValueError(f"unknown field type for field {field_id}: {kind!r}")
            self.fields[field_id] = kind

    def kind(self, field_id):
        return self.fields.get(field_id)

    def __repr__(self):
        return f"Schema({self.fields!r})"


def _check_value(field_id, kind, value):
    if kind in ("uint", "sint"):
        if isinstance(value, bool) or not isinstance(value, int):
            raise EncodeError(f"field {field_id} ({kind}) must be an int, not {type(value).__name__}")
        if kind == "uint" and not 0 <= value <= UINT64_MAX:
            raise EncodeError(f"field {field_id} (uint) out of range: {value}")
        if kind == "sint" and not SINT64_MIN <= value <= SINT64_MAX:
            raise EncodeError(f"field {field_id} (sint) out of range: {value}")
    elif kind == "bytes":
        if not isinstance(value, (bytes, bytearray)):
            raise EncodeError(f"field {field_id} (bytes) must be bytes, not {type(value).__name__}")
    elif kind == "str":
        if not isinstance(value, str):
            raise EncodeError(f"field {field_id} (str) must be a str, not {type(value).__name__}")


def _encode_value(kind, value):
    if kind == "uint":
        return encode_uvarint(value)
    if kind == "sint":
        return encode_svarint(value)
    if kind == "bytes":
        body = bytes(value)
    elif kind == "str":
        body = value.encode("utf-8")
    else:
        body = _encode_series(value)
    return encode_uvarint(len(body)) + body


def _encode_series(values):
    """Packed body of a series. A series object written into many records is encoded only once."""
    key = id(values)
    cached = _series_cache.get(key)
    if cached is not None and cached[0] is values:
        return cached[1]
    body = encode_deltas(values)
    if len(_series_cache) >= SERIES_CACHE_SIZE:
        _series_cache.clear()
    _series_cache[key] = (values, body)
    return body


def encode_record(schema, values):
    """Encode `values` (field id -> value) in ascending field id order; fields that are None are left out."""
    out = bytearray()
    for field_id in sorted(values):
        value = values[field_id]
        kind = schema.kind(field_id)
        if kind is None:
            raise EncodeError(f"field {field_id} is not in the schema")
        if value is None:
            continue
        _check_value(field_id, kind, value)
        field = encode_uvarint((field_id << 3) | FIELD_TYPES[kind])
        field += _encode_value(kind, value)
        out += field
    return bytes(out)


def _read_length_delimited(data, index):
    length, start = decode_uvarint(data, index)
    end = start + length
    if end > len(data):
        raise TruncatedError(f"field of {length} bytes runs past the end of the record")
    return start, end


def _decode_value(kind, data, index):
    if kind == "uint":
        value, index = decode_uvarint(data, index)
        if value > UINT64_MAX:
            raise DecodeError(f"uint out of the 64-bit range: {value}")
        return value, index
    if kind == "sint":
        return decode_svarint(data, index)
    start, end = _read_length_delimited(data, index)
    if kind == "bytes":
        return bytes(data[start:end]), end
    if kind == "str":
        try:
            return bytes(data[start:end]).decode("utf-8"), end
        except UnicodeDecodeError as error:
            raise DecodeError(f"str field is not valid UTF-8: {error}") from None
    return decode_deltas(data, start, end), end


def _skip(wire_type, data, index):
    if wire_type == WIRE_VARINT:
        return decode_uvarint(data, index)[1]
    if wire_type == WIRE_BYTES:
        return _read_length_delimited(data, index)[1]
    raise DecodeError(f"unknown wire type {wire_type}")


def decode_record(schema, data):
    """Decode a record into a dict of the fields present. Unknown fields are skipped; a repeated field keeps its last value."""
    values = {}
    index = 0
    while index < len(data):
        key, index = decode_uvarint(data, index)
        field_id, wire_type = key >> 3, key & 0x7
        if field_id == 0:
            raise DecodeError("field id 0 is reserved")
        kind = schema.kind(field_id)
        if kind is None:
            index = _skip(wire_type, data, index)
            continue
        if FIELD_TYPES[kind] != wire_type:
            raise DecodeError(f"field {field_id} ({kind}) has wire type {wire_type}")
        values[field_id], index = _decode_value(kind, data, index)
    return values
