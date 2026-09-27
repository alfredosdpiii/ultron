"""Reporting date windows (see SPEC.md)."""
from .months import add_months
from .parse import parse_kind
from .window import shift, window

__all__ = ["add_months", "parse_kind", "shift", "window"]
