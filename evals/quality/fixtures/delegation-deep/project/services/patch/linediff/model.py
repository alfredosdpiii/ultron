"""Data types shared by the diff, format and apply modules."""
from dataclasses import dataclass, field
from typing import NamedTuple

TAGS = ("equal", "delete", "insert")
LINE_TAGS = (" ", "-", "+")


class Op(NamedTuple):
    """One step of an edit script: a[i1:i2] is kept, deleted, or b[j1:j2] is inserted."""

    tag: str
    i1: int
    i2: int
    j1: int
    j2: int

    def old_len(self):
        return self.i2 - self.i1

    def new_len(self):
        return self.j2 - self.j1


class PatchError(Exception):
    """A hunk could not be applied. `index` is the 0-based position of the hunk in the patch."""

    def __init__(self, index, message):
        super().__init__(f"hunk {index + 1}: {message}")
        self.index = index


@dataclass
class Hunk:
    """A unified-diff hunk. Starts follow the unified header convention (see SPEC.md)."""

    old_start: int
    old_count: int
    new_start: int
    new_count: int
    lines: list = field(default_factory=list)

    def old_lines(self):
        return [text for tag, text in self.lines if tag != "+"]

    def new_lines(self):
        return [text for tag, text in self.lines if tag != "-"]

    def old_anchor(self):
        """0-based index in the old file where the hunk's old lines begin."""
        return self.old_start - 1 if self.old_count else self.old_start

    def new_anchor(self):
        """0-based index in the new file where the hunk's new lines begin."""
        return self.new_start - 1 if self.new_count else self.new_start

    def header(self):
        return f"@@ -{self.old_start},{self.old_count} +{self.new_start},{self.new_count} @@"

    def changed(self):
        return sum(1 for tag, _ in self.lines if tag != " ")

    def check(self):
        """Raise ValueError when the header counts disagree with the body."""
        for tag, text in self.lines:
            if tag not in LINE_TAGS:
                raise ValueError(f"bad line tag {tag!r}")
            if "\n" in text:
                raise ValueError("a line must not contain a newline")
        if len(self.old_lines()) != self.old_count or len(self.new_lines()) != self.new_count:
            raise ValueError(f"{self.header()} does not match its {len(self.lines)} body lines")
        if self.old_start < 0 or self.new_start < 0:
            raise ValueError(f"{self.header()} has a negative start")
