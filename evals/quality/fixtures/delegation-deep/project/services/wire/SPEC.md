# wire: binary codec and framed container (package `frames`)

Everything is importable from `frames`. Every error is a `WireError` (a `ValueError`): `EncodeError` for values
that cannot be encoded, `DecodeError` for invalid bytes, with subclasses `TruncatedError` (the bytes end before the
value is complete) and `StreamError` (framed streams; subclasses `BadHeader`, `CorruptFrame`, `TruncatedStream`).

## Varints

- `encode_uvarint(n)`: unsigned LEB128, seven bits per byte, least significant group first, the high bit set on
  every byte except the last; always the shortest encoding (`0` is `b"\x00"`, `300` is `b"\xac\x02"`). Accepts
  ints `0 <= n < 2**70` (at most 10 bytes); anything else (including `bool`) raises `EncodeError`.
- `decode_uvarint(data, pos=0)` returns `(value, position after the varint)`. `TruncatedError` if the data ends
  while the high bit is still set; `DecodeError` if the varint runs to an 11th byte, or if it has more than one
  byte and its last byte is `0x00` (not minimal).
- `zigzag(n)` maps any Python int to a non-negative one: `n >= 0` becomes `2n`, `n < 0` becomes `-2n - 1`
  (0, -1, 1, -2, 2 become 0, 1, 2, 3, 4). There is no range limit. `unzigzag` is its inverse.
- `encode_svarint(n)`: `n` must be a signed 64-bit int (`SINT64_MIN = -2**63` to `SINT64_MAX = 2**63 - 1`), else
  `EncodeError`; encoded as `encode_uvarint(zigzag(n))`. `encode_svarint(-1) == b"\x01"`,
  `encode_svarint(64) == b"\x80\x01"`. `decode_svarint(data, pos=0)` is the inverse and raises `DecodeError` for
  a value outside the 64-bit range.

## Packed series

- `encode_deltas(values)`: a list or tuple of signed 64-bit ints (else `EncodeError`). Written as
  `uvarint(len(values))`, then for each value `uvarint(zigzag(value - previous))`, where `previous` starts at 0.
  Differences can exceed 64 bits (e.g. from `-2**63` to `2**63 - 1`) and are encoded exactly.
  `encode_deltas([100, 98, 98]) == b"\x03\xc8\x01\x03\x00"`; `encode_deltas([]) == b"\x00"`.
- `decode_deltas(data, pos=0, end=None)` decodes a series that uses exactly `data[pos:end]` (`end` defaults to
  `len(data)`) and returns a list. `TruncatedError` if a count or value runs past `end` or the count exceeds the
  bytes left; `DecodeError` if bytes remain after the last value or a value leaves the 64-bit range.

## Records

- `Schema(fields)`: `fields` maps field ids (ints `1 .. 2**29 - 1`) to a type: `"uint"` (0 .. 2**64 - 1), `"sint"`
  (signed 64-bit), `"bytes"`, `"str"` (UTF-8) or `"sints"` (a packed series). Invalid ids or types: `ValueError`.
- `encode_record(schema, values)`: `values` maps field ids to values. Fields are written in ascending id order;
  a value of `None` is left out. Each field is a key `uvarint((id << 3) | wire_type)` then the value: wire type 0
  for `uint` (a uvarint) and `sint` (a svarint), wire type 2 for the others (`uvarint(len(body))` then the body:
  the raw bytes, the UTF-8 text, or `encode_deltas(list)`). An id not in the schema, a value of the wrong type
  or out of its range: `EncodeError`. Example: `Schema({1: "uint", 2: "str", 3: "sints"})` with
  `{1: 150, 2: "hi", 3: [1, -1]}` encodes to `b"\x08\x96\x01\x12\x02hi\x1a\x03\x02\x02\x03"`.
- `decode_record(schema, data)` returns a dict of the fields present (`bytes` values as `bytes`, `sints` as a
  list). Unknown ids are skipped by wire type (0 and 2; any other wire type is a `DecodeError`); a repeated field
  keeps its last value. Field id 0, a known field with the wrong wire type, a `uint` above `2**64 - 1`, a `sint`
  outside 64 bits, or invalid UTF-8: `DecodeError`. A value running past the end: `TruncatedError`.

## Framed container

- A stream is the header `HEADER = b"WIRE" + bytes([1])` followed by frames. A frame is: the type byte
  (1-255), `uvarint(len(payload))`, the payload (at most `MAX_PAYLOAD = 65536` bytes), then the CRC32 (as
  `zlib.crc32`) of the type byte followed by the payload, 4 bytes big-endian.
  `encode_frame(1, b"") == b"\x01\x00\xa5\x05\xdf\x1b"`.
- `Frame(type, payload)` is a named tuple. `encode_frame(type, payload)` and `encode_stream(frames)` (the header
  then each frame in order; `frames` holds `(type, payload)` pairs) raise `EncodeError` for a type outside
  1-255, a payload that is not bytes, or one longer than `MAX_PAYLOAD`.
- `StreamDecoder()`: `feed(chunk)` appends bytes (any split, including empty chunks) and returns the list of
  frames completed so far and not yet returned, in stream order. The result never depends on how the stream is
  split into chunks. `close()` ends the stream.
  - Header: as soon as the bytes received differ from `HEADER` (or its prefix), `feed` raises `BadHeader`.
  - A frame whose CRC does not match is dropped: it is not returned, `dropped` is incremented, and decoding
    continues right after its CRC. `delivered` counts the frames returned.
  - Type byte 0, a length varint that is not valid (too long, not minimal) or a length above `MAX_PAYLOAD`:
    `feed` raises `CorruptFrame` (frames completed earlier in the same chunk are lost with it).
  - `close()` raises `TruncatedStream` if the header is incomplete (also for an empty stream) or bytes of an
    unfinished frame remain; otherwise it returns `None`.
  - After any error the decoder is unusable: every later `feed` or `close` raises `StreamError`. After a
    successful `close`, `feed` and `close` raise `StreamError` too.
- `decode_stream(data)` decodes a complete stream at once (one `feed`, then `close`) and returns the list of
  frames; errors propagate.
