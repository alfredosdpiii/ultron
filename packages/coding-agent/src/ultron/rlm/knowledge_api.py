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

`query(sql)` lets an investigator ask the whole repository a structural question in SQL, as CodexGraph's agent
does in Cypher (arXiv 2408.03910): who calls this name, which routes lack this guard, which migrations create a
trigger. It sees only the revision's views (`GRAPH_SCHEMA`), on a connection of its own, through an authorizer
that allows reading those views and nothing else, within a time and row limit; an error comes back as text the
investigator can fix its query from.
"""
from __future__ import annotations

import math
import posixpath
import secrets
import sqlite3
import time
import urllib.parse
from dataclasses import dataclass
from typing import Any

#: The store schema this reader understands (`knowledge::SCHEMA_VERSION`): 2 added the review notes, 3 the rules
#: (who wrote a note, whether this reviewer missed it, the fates of the findings that cited it), 4 the graph
#: (symbol ranges and parents, calls, imports).
SCHEMA_VERSION = 4

#: Rows a graph query returns, characters of a cell, characters of a query, and how long one may run.
QUERY_ROWS = 30
QUERY_CELL_CHARS = 200
QUERY_CHARS = 2000
QUERY_SECONDS = 5.0

#: The views a graph query reads, each the revision's rows only. `target` is the repository file an import names,
#: when it is one (relative TypeScript specifiers, Python modules).
GRAPH_VIEWS = {
    "files": "SELECT e.path AS path, b.lang AS lang FROM main.entries e JOIN main.blobs b ON b.oid = e.blob"
             " WHERE e.tree = {tree}",
    "symbols": "SELECT e.path AS path, s.name AS name, s.kind AS kind, s.parent AS parent, s.line AS line,"
               " s.end_line AS end_line FROM main.entries e JOIN main.symbols s ON s.blob = e.blob WHERE e.tree = {tree}",
    "calls": "SELECT e.path AS path, c.caller AS caller, c.callee AS callee, c.receiver AS receiver, c.line AS line"
             " FROM main.entries e JOIN main.calls c ON c.blob = e.blob WHERE e.tree = {tree}",
    "imports": "SELECT e.path AS path, i.module AS module, i.name AS name,"
               " resolve_import(e.path, i.module, i.name) AS target, i.line AS line"
               " FROM main.entries e JOIN main.imports i ON i.blob = e.blob WHERE e.tree = {tree}",
    "constructs": "SELECT e.path AS path, c.kind AS kind, c.line AS line, c.count AS count"
                  " FROM main.entries e JOIN main.constructs c ON c.blob = e.blob WHERE e.tree = {tree}",
}

#: The views as the investigators are told them.
GRAPH_SCHEMA = ("files(path, lang); symbols(path, name, kind, parent, line, end_line), kind function, class or "
                "method, parent the symbol containing it; calls(path, caller, callee, receiver, line), caller the "
                "symbol the call is made from, callee the called name; imports(path, module, name, target, line), "
                "target the repository file it names; constructs(path, kind, line, count), kind such as "
                "sql:trigger:before-update, ts:decorator:Get, py:call:execute")

#: Functions a graph query may not call.
_DENIED_FUNCTIONS = {"load_extension", "readfile", "writefile", "edit", "fts3_tokenizer", "zipfile",
                     "sqlar_compress", "sqlar_uncompress"}


class QueryError(Exception):
    """A graph query that could not run: the message says why, for the investigator to fix it."""


def _import_resolver(paths: set[str]):
    """`resolve_import(path, module, name)`: the repository file an import of `path` names, or None."""
    modules: dict[str, str] = {}
    for path in sorted(paths):
        if not path.endswith(".py"):
            continue
        parts = path[:-3].split("/")
        if parts[-1] == "__init__":
            parts = parts[:-1]
        for start in range(len(parts)):
            dotted = ".".join(parts[start:])
            if dotted and (dotted not in modules or start == 0):
                modules[dotted] = path

    def resolve(path: Any, module: Any, name: Any) -> str | None:
        if not isinstance(path, str) or not isinstance(module, str) or not module:
            return None
        name = name if isinstance(name, str) and name not in ("*", "default") else None
        if not path.endswith(".py"):
            if not module.startswith("."):
                return None
            base = posixpath.normpath(posixpath.join(posixpath.dirname(path), module))
            for candidate in (base, *(f"{base}{ext}" for ext in (".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs")),
                              *(f"{base}/index{ext}" for ext in (".ts", ".tsx", ".js"))):
                if candidate in paths:
                    return candidate
            return None
        if module.startswith("."):
            dots = len(module) - len(module.lstrip("."))
            directory = posixpath.dirname(path)
            for _ in range(dots - 1):
                directory = posixpath.dirname(directory)
            rest = module.lstrip(".")
            base = posixpath.join(directory, rest.replace(".", "/")) if rest else directory
            candidates = [f"{base}.py", f"{base}/__init__.py"]
            # `from . import name`: the name may be a module.
            if name:
                candidates = [f"{base}/{name}.py", f"{base}/{name}/__init__.py"] + candidates
            return next((candidate for candidate in candidates if candidate in paths), None)
        return (modules.get(f"{module}.{name}") if name else None) or modules.get(module)

    return resolve

#: A rule rejected at least this often, and accepted less than this share of the time, is no longer served.
DISCREDIT_REJECTED = 3
DISCREDIT_SHARE = 0.25
#: What a miss adds to a rule's rank: a person found there what this reviewer did not.
MISS_BONUS = 0.5

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
    #: `addressed` (its thread was resolved), `outdated` (a later commit changed the lines, unresolved) or `open`.
    state: str


@dataclass
class Rule:
    """A person's addressed review comment, as a rule the investigators can cite (`R<id>`), with its credit."""
    id: int
    note: Note
    #: The fates of the findings that cited it.
    accepted: int
    rejected: int
    #: This reviewer reviewed that pull request and raised nothing there.
    missed: bool
    #: The file it was left on, when it is not the file asked about: how much that file is like it.
    alike: "Similar | None" = None

    @property
    def credit(self) -> float:
        """Its rank: the share of citing findings accepted (Laplace), more for a miss."""
        return (self.accepted + 1) / (self.accepted + self.rejected + 2) + (MISS_BONUS if self.missed else 0.0)

    @property
    def discredited(self) -> bool:
        return self.rejected >= DISCREDIT_REJECTED and self.accepted < DISCREDIT_SHARE * (self.accepted + self.rejected)


@dataclass
class Similar:
    path: str
    score: float
    #: The shared constructs, rarest first; among equally rare ones, the most specific first.
    shared: list[str]


class Knowledge:
    """One repository at one revision, in the store."""

    def __init__(self, conn: sqlite3.Connection, tree: int, repo: int = 0, db: str = "") -> None:
        self._conn = conn
        self._tree = tree
        self._repo = repo
        self._db = db

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
        return cls(conn, int(tree[0]), int(tree[1]), db)

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


    def query(self, sql: Any, *, limit: int = QUERY_ROWS,
              seconds: float = QUERY_SECONDS) -> tuple[list[str], list[tuple[Any, ...]], bool]:
        """One read-only SELECT over the revision's graph (`GRAPH_VIEWS`): (columns, rows, whether more rows were
        left out). Raises QueryError with a message the investigator can act on."""
        if not isinstance(sql, str) or not sql.strip() or len(sql) > QUERY_CHARS or "\0" in sql:
            raise QueryError(f"a query is one SQL SELECT of at most {QUERY_CHARS} characters")
        try:
            conn = sqlite3.connect(f"file:{urllib.parse.quote(self._db)}?mode=ro", uri=True, timeout=5)
        except sqlite3.Error as error:
            raise QueryError(f"the graph is not available: {error}") from None
        try:
            if "imports" in sql.lower():
                paths = {str(path) for (path,) in conn.execute("SELECT path FROM entries WHERE tree = ?", (self._tree,))}
                conn.create_function("resolve_import", 3, _import_resolver(paths), deterministic=True)
            else:
                conn.create_function("resolve_import", 3, lambda *_: None, deterministic=True)
            # Two layers: only views under names this query cannot know read the tables (a common table
            # expression of the query may take any name, `symbols` included, and its reads are reported under it);
            # the views the query names read those.
            hidden = {name: f"g{secrets.token_hex(8)}_{name}" for name in GRAPH_VIEWS}
            for name, body in GRAPH_VIEWS.items():
                conn.execute(f"CREATE TEMP VIEW {hidden[name]} AS {body.format(tree=int(self._tree))}")
                conn.execute(f"CREATE TEMP VIEW {name} AS SELECT * FROM temp.{hidden[name]}")
            readers = set(hidden.values())

            def authorize(action: int, first: Any, second: Any, database: Any, source: Any) -> int:
                if action in (sqlite3.SQLITE_SELECT, sqlite3.SQLITE_RECURSIVE):
                    return sqlite3.SQLITE_OK
                if action == sqlite3.SQLITE_READ:
                    # The views, and the tables only through them: no other revision, no review notes. A
                    # common table expression of the query has no database.
                    allowed = database is None or database == "temp" or source in readers
                    return sqlite3.SQLITE_OK if allowed else sqlite3.SQLITE_DENY
                if action == sqlite3.SQLITE_FUNCTION:
                    return sqlite3.SQLITE_DENY if str(second).lower() in _DENIED_FUNCTIONS else sqlite3.SQLITE_OK
                return sqlite3.SQLITE_DENY

            deadline = time.monotonic() + seconds
            conn.set_progress_handler(lambda: 1 if time.monotonic() > deadline else 0, 2000)
            conn.set_authorizer(authorize)
            try:
                cursor = conn.execute(sql)
                columns = [str(item[0]) for item in cursor.description or []]
                if not columns:
                    raise QueryError("a query must be a SELECT that returns rows")
                rows = cursor.fetchmany(limit + 1)
            except sqlite3.Warning:
                raise QueryError("one statement only, without a trailing one") from None
            except sqlite3.DatabaseError as error:
                message = str(error)
                if "interrupted" in message:
                    raise QueryError(f"the query ran longer than {seconds:g}s: narrow it (a WHERE on path or name, "
                                     "fewer joins)") from None
                if "not authorized" in message or "prohibited" in message:
                    raise QueryError("not authorized: a query reads only the views files, symbols, calls, imports "
                                     "and constructs") from None
                raise QueryError(f"{message}. The views: {GRAPH_SCHEMA}") from None
        finally:
            conn.close()

        def cell(value: Any) -> Any:
            if isinstance(value, str) and len(value) > QUERY_CELL_CHARS:
                return value[:QUERY_CELL_CHARS] + "..."
            if isinstance(value, bytes):
                return f"<{len(value)} bytes>"
            return value

        return columns, [tuple(cell(value) for value in row) for row in rows[:limit]], len(rows) > limit

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

    def has_rules(self) -> bool:
        """Whether the store holds a rule of this repository: a person's addressed comment."""
        try:
            return self._conn.execute("SELECT 1 FROM notes WHERE repo = ? AND human = 1 AND state = 'addressed' LIMIT 1",
                                      (self._repo,)).fetchone() is not None
        except sqlite3.Error:
            return False

    def rules_for(self, path: str, *, limit: int = 8, exclude: set[str] | None = None) -> list[Rule]:
        """The rules for `path`: people's addressed comments on it and on the files most like it (`similar`), the
        discredited left out; the best credited first, then this file's, the closest file's, the newest."""
        rules = self._rules(path, None)
        for alike in self.similar(path, limit=6, exclude=exclude):
            rules += self._rules(alike.path, alike)
        rules = [rule for rule in rules if not rule.discredited]
        rules.sort(key=lambda rule: rule.note.at, reverse=True)
        rules.sort(key=lambda rule: -(rule.alike.score if rule.alike else math.inf))
        rules.sort(key=lambda rule: -rule.credit)
        return rules[:limit]

    def _rules(self, path: str, alike: "Similar | None") -> list[Rule]:
        try:
            rows = self._conn.execute(
                "SELECT n.id, n.pull, n.path, n.line, n.author, n.body, n.url, n.at, n.state, n.accepted, n.rejected,"
                " n.missed FROM notes n WHERE n.repo = ? AND n.path = ? AND n.human = 1 AND n.state = 'addressed'"
                " ORDER BY n.at DESC LIMIT 20",
                (self._repo, path),
            ).fetchall()
        except sqlite3.Error:
            return []
        return [Rule(int(rid), Note(int(pull), str(at_path), None if line is None else int(line), str(author), str(body),
                                    str(url), str(at), str(state)), int(accepted), int(rejected), bool(missed), alike)
                for rid, pull, at_path, line, author, body, url, at, state, accepted, rejected, missed in rows]

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
