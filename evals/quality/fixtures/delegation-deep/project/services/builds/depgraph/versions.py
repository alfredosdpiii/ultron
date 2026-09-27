"""Versions (MAJOR.MINOR.PATCH) and version constraints."""
import re
from typing import NamedTuple

_NUMBER = re.compile(r"^(0|[1-9][0-9]*)$")
_OPERATORS = (">=", "<=", ">", "<", "=", "^", "~")


def _components(text):
    """The numeric components of a full or partial version ("1", "1.2" or "1.2.3")."""
    if not isinstance(text, str):
        raise ValueError(f"not a version: {text!r}")
    parts = text.split(".")
    if not 1 <= len(parts) <= 3 or not all(_NUMBER.match(part) for part in parts):
        raise ValueError(f"not a version: {text!r}")
    return [int(part) for part in parts]


class Version(NamedTuple):
    major: int
    minor: int
    patch: int

    @classmethod
    def parse(cls, text):
        """A full version "MAJOR.MINOR.PATCH"; anything else raises ValueError."""
        parts = _components(text)
        if len(parts) != 3:
            raise ValueError(f"a version needs three components: {text!r}")
        return cls(*parts)

    @classmethod
    def padded(cls, parts):
        return cls(*(list(parts) + [0, 0, 0])[:3])

    def __str__(self):
        return f"{self.major}.{self.minor}.{self.patch}"


def _caret_upper(parts):
    """The first version a caret range excludes: the next change to the leftmost non-zero given component."""
    major = parts[0]
    if major > 0 or len(parts) == 1:
        return Version(major + 1, 0, 0)
    minor = parts[1]
    if minor > 0 or len(parts) == 2:
        return Version(0, minor + 1, 0)
    return Version(0, 0, parts[2] + 1)


def _tilde_upper(parts):
    if len(parts) == 1:
        return Version(parts[0] + 1, 0, 0)
    return Version(parts[0], parts[1] + 1, 0)


def _clauses(token):
    """The (operator, version) comparisons one constraint token stands for."""
    if token == "*":
        return []
    operator = next((op for op in _OPERATORS if token.startswith(op)), "")
    rest = token[len(operator):]
    parts = _components(rest)
    if operator in ("", "="):
        if len(parts) != 3:
            raise ValueError(f"an exact version needs three components: {token!r}")
        return [("==", Version(*parts))]
    lower = Version.padded(parts)
    if operator == "^":
        return [(">=", lower), ("<", _caret_upper(parts))]
    if operator == "~":
        return [(">=", lower), ("<", _tilde_upper(parts))]
    return [(operator, lower)]


_COMPARE = {
    "==": lambda a, b: a == b,
    ">=": lambda a, b: a >= b,
    "<=": lambda a, b: a <= b,
    ">": lambda a, b: a > b,
    "<": lambda a, b: a < b,
}


class Constraint:
    """A space-separated conjunction of constraint tokens, e.g. "^1.2", ">=1.0 <2.0", "~0.3.1", "1.4.2", "*"."""

    def __init__(self, text):
        if not isinstance(text, str) or not text.split():
            raise ValueError(f"not a constraint: {text!r}")
        self.text = " ".join(text.split())
        self._comparisons = [clause for token in self.text.split(" ") for clause in _clauses(token)]

    def matches(self, version):
        """Whether `version` (a Version or a full version string) satisfies every token."""
        if isinstance(version, str):
            version = Version.parse(version)
        return all(_COMPARE[op](version, bound) for op, bound in self._comparisons)

    def best(self, versions):
        """The highest of `versions` (strings) that satisfies the constraint, or None."""
        matching = [Version.parse(text) for text in versions if self.matches(text)]
        return str(max(matching)) if matching else None

    def __eq__(self, other):
        return isinstance(other, Constraint) and self.text == other.text

    def __hash__(self):
        return hash(self.text)

    def __repr__(self):
        return f"Constraint({self.text!r})"
