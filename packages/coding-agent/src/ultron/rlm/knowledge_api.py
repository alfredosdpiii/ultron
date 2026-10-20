"""The knowledge graph, read from the kernel.

The host (`crates/knowledge`) indexes every repository Ultron reviews into one SQLite store: for each git blob, the
constructs it is made of (`sql:trigger:before-update`, `ts:decorator:Controller`, `ts:call:runWithBypass`) and the
symbols it defines; for each revision, its path-to-blob table. This module only reads it, read-only, and only a
schema version it knows; anything else (no store, no such repository or revision, another version) is no
knowledge, never an error, so a review without the store runs as before.

`similar(path)` answers "which other files do what this file does": files of the same language that share the
file's rare constructs, each construct weighted by how rare it is in that revision (inverse document frequency).
Names are not similarity: every Prisma migration is called `migration.sql`, and the one that matters for a new
append-only table is the other migration with a refusing `BEFORE UPDATE` trigger.
"""
from __future__ import annotations

import math
import sqlite3
import urllib.parse
from dataclasses import dataclass
from typing import Any

#: The store schema this reader understands (`knowledge::SCHEMA_VERSION`): 2 added the review notes.
SCHEMA_VERSION = 2

#: A construct shared by more than this share of a language's files says nothing about similarity.
COMMON_SHARE = 0.5


@dataclass
class Note:
    """A review comment left on a pull request of the repository, by anyone (a teammate, a bot, this reviewer)."""
    pull: int
    path: str
    line: int | None
    author: str
    body: str
    url: str
    at: str
    #: `addressed` (its thread was resolved, or a later commit changed the lines) or `open`.
    state: str


@dataclass
class Similar:
    path: str
    score: float
    #: The shared constructs, rarest first; among equally rare ones, the most specific first.
    shared: list[str]


class Knowledge:
    """One repository at one revision, in the store."""

    def __init__(self, conn: sqlite3.Connection, tree: int, repo: int = 0) -> None:
        self._conn = conn
        self._tree = tree
        self._repo = repo

    @classmethod
    def open(cls, db: Any, repo: Any, rev: Any) -> "Knowledge | None":
        """The revision `rev` of `repo` in the store at `db`; None when any of them is missing or unreadable."""
        if not all(isinstance(value, str) and value for value in (db, repo, rev)):
            return None
        try:
            conn = sqlite3.connect(f"file:{urllib.parse.quote(db)}?mode=ro", uri=True, timeout=5)
        except sqlite3.Error:
            return None
        try:
            row = conn.execute("SELECT value FROM meta WHERE key = 'schema_version'").fetchone()
            if row is None or str(row[0]) != str(SCHEMA_VERSION):
                conn.close()
                return None
            tree = conn.execute(
                "SELECT t.id, t.repo FROM trees t JOIN repos r ON r.id = t.repo WHERE r.name = ? AND t.rev = ?",
                (repo, rev),
            ).fetchone()
        except sqlite3.Error:
            conn.close()
            return None
        if tree is None:
            conn.close()
            return None
        return cls(conn, int(tree[0]), int(tree[1]))

    def close(self) -> None:
        self._conn.close()

    def constructs(self, path: str) -> list[tuple[str, int]]:
        """(kind, first line) of the file at `path`."""
        try:
            return [(str(kind), int(line)) for kind, line in self._conn.execute(
                "SELECT c.kind, c.line FROM entries e JOIN constructs c ON c.blob = e.blob"
                " WHERE e.tree = ? AND e.path = ? ORDER BY c.line, c.kind",
                (self._tree, path),
            )]
        except sqlite3.Error:
            return []

    def similar(self, path: str, *, limit: int = 3, exclude: set[str] | None = None) -> list[Similar]:
        """The files most like `path` by shared rare constructs, best first; [] when nothing is shared."""
        excluded = set(exclude or ()) | {path}
        try:
            own = self._conn.execute(
                "SELECT e.blob, b.lang FROM entries e JOIN blobs b ON b.oid = e.blob WHERE e.tree = ? AND e.path = ?",
                (self._tree, path),
            ).fetchone()
            if own is None:
                return []
            blob, lang = own
            mine = [str(kind) for (kind,) in self._conn.execute("SELECT kind FROM constructs WHERE blob = ?", (blob,))]
            if not mine:
                return []
            (files,) = self._conn.execute(
                "SELECT COUNT(*) FROM entries e JOIN blobs b ON b.oid = e.blob WHERE e.tree = ? AND b.lang = ?",
                (self._tree, lang),
            ).fetchone()
            marks = ",".join("?" * len(mine))
            frequency = dict(self._conn.execute(
                f"SELECT c.kind, COUNT(*) FROM entries e JOIN blobs b ON b.oid = e.blob"
                f" JOIN constructs c ON c.blob = e.blob"
                f" WHERE e.tree = ? AND b.lang = ? AND c.kind IN ({marks}) GROUP BY c.kind",
                (self._tree, lang, *mine),
            ).fetchall())
            weight = {kind: math.log((files + 1) / (count + 1)) for kind, count in frequency.items()
                      if count > 1 and count <= COMMON_SHARE * files}
            weight = {kind: value for kind, value in weight.items() if value > 0}
            if not weight:
                return []
            rare = sorted(weight)
            marks = ",".join("?" * len(rare))
            shared: dict[str, list[str]] = {}
            for other, kind in self._conn.execute(
                f"SELECT e.path, c.kind FROM entries e JOIN blobs b ON b.oid = e.blob"
                f" JOIN constructs c ON c.blob = e.blob"
                f" WHERE e.tree = ? AND b.lang = ? AND c.kind IN ({marks})",
                (self._tree, lang, *rare),
            ):
                if other not in excluded:
                    shared.setdefault(str(other), []).append(str(kind))
        except sqlite3.Error:
            return []
        ranked = []
        for other, kinds in shared.items():
            # `sql:trigger:before-update` says more than `sql:trigger`, which every such file also has.
            kinds.sort(key=lambda kind: (-weight[kind], -kind.count(":"), kind))
            ranked.append(Similar(other, round(sum(weight[kind] for kind in kinds), 3), kinds))
        ranked.sort(key=lambda item: (-item.score, len(item.shared), item.path))
        return ranked[:limit]


    def has_notes(self) -> bool:
        """Whether the store holds any review note of this repository."""
        try:
            return self._conn.execute("SELECT 1 FROM notes WHERE repo = ? LIMIT 1", (self._repo,)).fetchone() is not None
        except sqlite3.Error:
            return False

    def notes_on(self, path: str, *, limit: int = 8) -> list[Note]:
        """Review comments left on `path` in earlier pull requests, addressed ones first, newest first."""
        return self._notes("n.path = ?", (path,), limit)

    def notes_like(self, path: str, *, limit: int = 8, exclude: set[str] | None = None) -> list[tuple[Note, Similar]]:
        """Addressed review comments on the files most like `path` (`similar`): what reviewers asked of code that
        does what this file does, with the constructs the two files share. Best match first."""
        out: list[tuple[Note, Similar]] = []
        for alike in self.similar(path, limit=6, exclude=exclude):
            for note in self._notes("n.path = ? AND n.state = 'addressed'", (alike.path,), limit):
                out.append((note, alike))
        # The closest file first; among its notes, the newest.
        out.sort(key=lambda pair: pair[0].at, reverse=True)
        out.sort(key=lambda pair: -pair[1].score)
        return out[:limit]

    def _notes(self, where: str, args: tuple[Any, ...], limit: int) -> list[Note]:
        try:
            rows = self._conn.execute(
                "SELECT n.pull, n.path, n.line, n.author, n.body, n.url, n.at, n.state FROM notes n"
                f" WHERE n.repo = ? AND {where}"
                " ORDER BY n.state = 'addressed' DESC, n.at DESC LIMIT ?",
                (self._repo, *args, limit),
            ).fetchall()
        except sqlite3.Error:
            # A store from before notes (schema 1) has none.
            return []
        return [Note(int(pull), str(path), None if line is None else int(line), str(author), str(body), str(url),
                     str(at), str(state)) for pull, path, line, author, body, url, at, state in rows]


def construct_label(kind: str) -> str:
    """`sql:trigger:before-update` as `trigger before-update`: the language prefix is the file's own."""
    _, _, rest = kind.partition(":")
    return rest.replace(":", " ") if rest else kind
