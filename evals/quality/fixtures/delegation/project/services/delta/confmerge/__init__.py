"""Layered configuration: defaults, config file, environment (see SPEC.md)."""
from .env import env_overrides
from .loader import load
from .merge import deep_merge
from .types import coerce

__all__ = ["coerce", "deep_merge", "env_overrides", "load"]
