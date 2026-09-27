"""Exception types raised by the codec. Every one is a ValueError."""


class WireError(ValueError):
    """Base class for every codec error."""


class EncodeError(WireError):
    """A value cannot be encoded: wrong type, or outside the range its encoding allows."""


class DecodeError(WireError):
    """Bytes that are not a valid encoding."""


class TruncatedError(DecodeError):
    """The bytes end before the value they start is complete."""


class StreamError(DecodeError):
    """A framed stream cannot be decoded any further."""


class BadHeader(StreamError):
    """The stream does not start with the container header."""


class CorruptFrame(StreamError):
    """A frame header is invalid, so the rest of the stream cannot be located."""


class TruncatedStream(StreamError):
    """The stream ended in the middle of the header or of a frame."""
