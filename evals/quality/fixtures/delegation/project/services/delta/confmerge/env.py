"""Environment variables to an override dict."""
from .types import coerce

PREFIX = "APP__"


def env_overrides(environ, prefix=PREFIX):
    tree = {}
    for name in sorted(environ):
        if not name.startswith(prefix):
            continue
        path = [segment.lower() for segment in name[len(prefix):].split("_")]
        if not all(path):
            continue
        node = tree
        for segment in path[:-1]:
            child = node.get(segment)
            if not isinstance(child, dict):
                child = node[segment] = {}
            node = child
        node[path[-1]] = coerce(environ[name])
    return tree
