"""Memo text helpers."""


def wrap_memo(text, width):
    """Wrap a memo into lines of at most `width` characters."""
    return [text[start : start + width] for start in range(0, len(text), width)] or [""]


def clean_memo(text):
    """A memo as stored: surrounding whitespace removed."""
    return text.strip()
