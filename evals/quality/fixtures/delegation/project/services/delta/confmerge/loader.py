"""The three configuration layers."""
from .env import env_overrides
from .merge import deep_merge


def load(defaults, file_config=None, environ=None):
    config = deep_merge(defaults, file_config or {})
    return deep_merge(config, env_overrides(environ or {}))
