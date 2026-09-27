"""LRU cache with per-entry expiry over integer ticks (see SPEC.md)."""
from .cache import TTLCache

__all__ = ["TTLCache"]
