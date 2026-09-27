"""Incremental decoding of a framed stream that arrives in chunks of any size."""
from .container import HEADER, MAX_PAYLOAD, Frame, frame_crc
from .errors import BadHeader, CorruptFrame, DecodeError, StreamError, TruncatedError, TruncatedStream
from .varint import decode_uvarint

COMPACT_AT = 4096

_DROPPED = object()


class StreamDecoder:
    """Feed chunks with feed(); each call returns the frames that chunk completed. Call close() at the end.

    Frames with a CRC mismatch are dropped and counted in `dropped`. Any other error is final: the decoder raises
    it, and every later call raises StreamError.
    """

    def __init__(self):
        self._buf = bytearray()
        self._pos = 0
        self._header_ok = False
        self._pending = None
        self._failed = None
        self._closed = False
        self.dropped = 0
        self.delivered = 0

    def feed(self, chunk):
        self._check_usable()
        if self._pos >= COMPACT_AT:
            self._compact()
        self._buf += chunk
        try:
            return self._drain()
        except StreamError as error:
            self._failed = error
            raise

    def close(self):
        """End of stream: raises TruncatedStream if the header or a frame is incomplete."""
        self._check_usable()
        self._closed = True
        if not self._header_ok:
            self._failed = TruncatedStream(f"stream ended after {len(self._buf)} header bytes")
            raise self._failed
        if self._pending is not None or self._pos < len(self._buf):
            self._failed = TruncatedStream(f"stream ended inside a frame ({len(self._buf) - self._pos} bytes left)")
            raise self._failed

    def _check_usable(self):
        if self._failed is not None:
            raise StreamError(f"decoder already failed: {self._failed}")
        if self._closed:
            raise StreamError("decoder is closed")

    def _compact(self):
        del self._buf[:self._pos]
        self._pos = 0

    def _drain(self):
        frames = []
        if not self._header_ok:
            seen = bytes(self._buf[: len(HEADER)])
            if seen != HEADER[: len(seen)]:
                raise BadHeader(f"stream does not start with {HEADER!r}: {seen!r}")
            if len(seen) < len(HEADER):
                return frames
            self._pos = len(HEADER)
            self._header_ok = True
        while True:
            frame = self._next_frame()
            if frame is None:
                return frames
            if frame is not _DROPPED:
                frames.append(frame)

    def _read_header(self):
        start = self._pos
        if start >= len(self._buf):
            return None
        frame_type = self._buf[start]
        if frame_type == 0:
            raise CorruptFrame(f"frame type 0 at offset {start}")
        try:
            length, body_at = decode_uvarint(self._buf, start + 1)
        except TruncatedError:
            return None
        except DecodeError as error:
            raise CorruptFrame(f"bad frame length at offset {start}: {error}") from None
        if length > MAX_PAYLOAD:
            raise CorruptFrame(f"frame length {length} exceeds {MAX_PAYLOAD}")
        return frame_type, length, body_at

    def _next_frame(self):
        if self._pending is None:
            self._pending = self._read_header()
            if self._pending is None:
                return None
        frame_type, length, body_at = self._pending
        end = body_at + length
        if len(self._buf) < end + 4:
            return None
        payload = bytes(self._buf[body_at:end])
        crc = int.from_bytes(self._buf[end : end + 4], "big")
        self._pending = None
        self._pos = end + 4
        if crc != frame_crc(frame_type, payload):
            self.dropped += 1
            return _DROPPED
        self.delivered += 1
        return Frame(frame_type, payload)
