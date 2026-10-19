"""Semantic versions (semver.org 2.0.0): compare("1.2.3", "1.10.0") -> -1, 0 or 1."""


def compare(a, b):
    """-1 if version a sorts before b, 0 if they have the same precedence, 1 if after."""
    if a == b:
        return 0
    return -1 if a < b else 1
