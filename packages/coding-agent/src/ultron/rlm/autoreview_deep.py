"""The deep pass of `ultron autoreview`: investigation beyond the diff, by retrieval, with nothing executed.

The fast pass (autoreview_api.py) reviews the changed lines. Many real defects are only visible from the code
around them: a helper that returns a sentinel instead of raising, a parametrize list in another file that the new
members are missing from, a comment that claims what the code does not do. This pass looks there.

1. Map (no model). From the diff: the symbols it defines, changes and calls, its constants, environment
   variables, flags and table names, and the claims its comments and documents make. From the repository at the
   head commit: who uses each symbol, what the called helpers do, which tests mention them and how those test
   files are parametrized, sibling files, and the docs and configs that name them. Rendered as a bounded
   "investigation brief" with file:line anchors.
2. Investigators. One tool-less frame per part of a claim-driven method (claims, siblings, deployment, tests,
   inputs) gets the diff, the brief and the fast pass's findings as leads, and replies with findings and
   *requests*: read, grep, list, definition, references, and three history lookups (history of a file, of a line
   range, of a string). The host validates each request, answers it from the head commit, appends the results as
   untrusted data and asks again, for a bounded number of rounds. An investigator that stops in its first round
   having looked at almost nothing is sent back once.
3. Evidence. Every finding cites file:line with the quoted source line. The host checks each quote at its line;
   a finding with a wrong quote, or with none, is dropped.

Read-only by construction: models have no tools, and the host runs only `git grep`, `git show <rev>:<path>`,
`git ls-tree` and `git log` (see `Repo`). No code of the reviewed repository is ever executed.
"""
from __future__ import annotations

import asyncio
import json
import posixpath
import re
from dataclasses import dataclass, field
from typing import Any, Callable

from infer_api import FrameError, Incomplete
from review_api import (
    _CALL,
    _COMMON,
    _DEF,
    _TEST,
    FileDiff,
    Runner,
    _clip,
    _run_process,
    _text,
    file_kind,
    normalize_category,
    similar_claims,
    skip_reason,
)
import autoreview_reference as reference_module
import autoreview_tests as testing
from review_prompts import DEEP_LENSES, DEEP_NUDGE, DEEP_SHAPE_NUDGE, DEEP_SHAPES_HEADER, DEEP_TRACE_NUDGE, deep_task

GIT_TIMEOUT_S = 30
#: The only git subcommands the deep pass runs. All of them only read the object database.
ALLOWED_GIT = ("grep", "show", "ls-tree", "log")

DEFAULT_ROUNDS = 4
MAX_ROUNDS = 8
MAX_REQUESTS = 8
ROUND_CHARS = 24_000
#: A ledger line of an earlier result, re-readable by `recall`.
LEDGER_LINE_CHARS = 160
MAX_READ_LINES = 200
MAX_GREP_HITS = 50
DEFAULT_GREP_HITS = 20
#: Hits fetched for a scoped or count-only grep: its count is exact up to this many.
MAX_COUNT_HITS = 1_000
MAX_REFERENCES = 30
MAX_LIST = 200
MAX_PATTERN_CHARS = 200
MAX_FILE_BYTES = 2_000_000
BRIEF_CHARS = 8_000
DIFF_CHARS = 40_000
LEADS_CHARS = 4_000
MAX_DEEP_FINDINGS = 8
MAX_CITATIONS = 6
CITATION_SLACK = 2
CITED_WINDOW = 6
DEDUPE_WINDOW = 3
MERGE_WINDOW = 5
LEVEL_ORDER = ("critical", "high", "medium", "low", "nit")
LEVEL_TO_OLD = {"critical": "blocker", "high": "major", "medium": "minor", "low": "minor", "nit": "nit"}
#: An investigator that is done in round 1 with fewer served lookups than this is sent back once.
MIN_LOOKUPS = 3
MAX_HISTORY = 20
MAX_BLAME_LINES = 60

_CITATION = {
    "type": "object",
    "properties": {"path": {"type": "string"}, "line": {"type": "integer"}, "quote": {"type": "string"}},
    "required": ["path", "line", "quote"],
}

DEEP_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {
        "findings": {
            "type": "array",
            "maxItems": MAX_DEEP_FINDINGS,
            "items": {
                "type": "object",
                "properties": {
                    "file": {"type": "string"},
                    "line": {"type": "integer"},
                    "severity": {"enum": ["critical", "high", "medium", "low", "nit", "blocker", "major", "minor"]},
                    "category": {"type": "string"},
                    "claim": {"type": "string"},
                    "why": {"type": "string"},
                    "scenario": {"type": "string"},
                    "suggested_fix": {"type": "string"},
                    "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                    "evidence": {"type": "array", "items": _CITATION},
                    "unpinned": {"type": ["object", "null"]},
                    "consequence": {"type": "string"},
                },
                "required": ["file", "line", "severity", "category", "claim", "why", "scenario", "evidence"],
            },
        },
        "requests": {"type": "array", "items": {"type": "object"}},
        "checked": {"type": "array", "items": {"type": "string"}},
        "done": {"type": "boolean"},
    },
    "required": ["findings", "requests", "done"],
}


class Rejected(Exception):
    """A request the host will not serve; the message goes back to the investigator."""


# --- The repository, read-only ---------------------------------------------------------------------------------


class Repo:
    """A repository at one commit, through a fixed set of read-only git subcommands."""

    def __init__(self, root: str, rev: str, runner: Runner | None = None) -> None:
        self.root = root
        self.rev = rev
        self._runner = runner or _run_process
        self._files: list[str] | None = None
        self._tracked: set[str] = set()
        self._cache: dict[str, list[str] | None] = {}
        #: Git calls made, for the record.
        self.calls = 0
        #: The structural reference of the change (autoreview_reference.Reference), once the map built it.
        self.reference: Any = None

    def _git(self, *args: str) -> tuple[int, str, str]:
        if not args or args[0] not in ALLOWED_GIT:
            raise AssertionError(f"the deep pass does not run git {args[:1]}")
        self.calls += 1
        return self._runner(["git", *args], self.root, GIT_TIMEOUT_S)

    def files(self) -> list[str]:
        if self._files is None:
            code, stdout, _ = self._git("ls-tree", "-r", "--name-only", "-z", self.rev)
            self._files = [name for name in stdout.split("\0") if name] if code == 0 else []
            self._tracked = set(self._files)
        return self._files

    def path(self, raw: Any) -> str:
        """`raw` as a tracked path of the commit, or Rejected."""
        if not isinstance(raw, str) or not raw.strip() or "\0" in raw or "\n" in raw:
            raise Rejected("path must be a string")
        text = raw.strip()
        if text.startswith(("/", "~", ":", "-")) or "\\" in text:
            raise Rejected(f"{text!r} is not a path inside the repository")
        if text.startswith("./"):
            text = text[2:]
        if ".." in text.split("/"):
            raise Rejected(f"{text!r} leaves the repository")
        normal = posixpath.normpath(text)
        self.files()
        if normal not in self._tracked:
            raise Rejected(f"{normal} is not a tracked file at the reviewed commit")
        return normal

    def lines(self, path: str) -> list[str] | None:
        """The lines of a tracked text file at the commit; None when it is binary, too large or unreadable."""
        if path not in self._cache:
            code, stdout, _ = self._git("show", f"{self.rev}:{path}")
            ok = code == 0 and "\0" not in stdout[:8192] and len(stdout) <= MAX_FILE_BYTES
            self._cache[path] = stdout.splitlines() if ok else None
        return self._cache[path]

    def grep(self, pattern: str, *, fixed: bool = False, word: bool = False, pathspec: str | None = None,
             limit: int = DEFAULT_GREP_HITS) -> list[tuple[str, int, str]]:
        """(path, line, text) of matching lines in tracked text files, at most `limit`."""
        args = ["grep", "-n", "-I", "--no-color", "-F" if fixed else "-E"]
        if word:
            args.append("-w")
        args += ["-e", pattern, self.rev, "--", pathspec or "."]
        code, stdout, stderr = self._git(*args)
        if code not in (0, 1):
            raise Rejected(f"grep failed: {_text(stderr, 120)}")
        hits: list[tuple[str, int, str]] = []
        prefix = self.rev + ":"
        for raw in stdout.splitlines():
            if raw.startswith(prefix):
                raw = raw[len(prefix):]
            path, _, rest = raw.partition(":")
            number, _, text = rest.partition(":")
            if number.isdigit():
                hits.append((path, int(number), text))
            if len(hits) >= limit:
                break
        return hits

    def history(self, path: str, count: int = 3) -> list[str]:
        """The last commits that touched a file: short hash, date, subject."""
        code, stdout, _ = self._git("log", "-n", str(count), "--format=%h %ad %s", "--date=short", self.rev, "--", path)
        return stdout.splitlines() if code == 0 else []

    def line_history(self, path: str, start: int, end: int, count: int = 5) -> list[str]:
        """The commits that last changed lines start..end of a file."""
        code, stdout, _ = self._git("log", f"-L{start},{end}:{path}", "-s", "-n", str(count), "--format=%h %ad %s",
                                    "--date=short", self.rev)
        return [line for line in stdout.splitlines() if line.strip()] if code == 0 else []

    def pickaxe(self, text: str, count: int = 5) -> list[str]:
        """The commits that added or removed a string."""
        code, stdout, _ = self._git("log", f"-S{text}", "-n", str(count), "--format=%h %ad %s", "--date=short", self.rev)
        return stdout.splitlines() if code == 0 else []

    def messages(self, base: str | None, count: int = 8) -> list[str]:
        """Subjects and first body lines of the commits under review."""
        if not base:
            return []
        code, stdout, _ = self._git("log", "-n", str(count), "--format=%s%n%b%x00", f"{base}..{self.rev}")
        out = []
        for entry in stdout.split("\0") if code == 0 else []:
            lines = [line.strip() for line in entry.strip().splitlines() if line.strip()]
            out += lines[:3]
        return out


_SYMBOL = re.compile(r"^[A-Za-z_][A-Za-z0-9_.$-]{0,79}$")
_GLOB = re.compile(r"^[A-Za-z0-9_./*?\[\]{}, -]{1,160}$")


def is_code(path: str) -> bool:
    """Whether a file holds code a test can exercise, as against configuration, manifests, scripts or prose."""
    return not _NON_CODE.search(path)


def _numbered(lines: list[str], start: int, end: int) -> str:
    return "\n".join(f"{number:>5} | {_clip(lines[number - 1], 300)}" for number in range(start, end + 1))


def _hits(hits: list[tuple[str, int, str]]) -> str:
    return "\n".join(f"{path}:{line}: {_clip(text.strip(), 200)}" for path, line, text in hits) or "0 matches"


def _definition_pattern(symbol: str) -> str:
    name = re.escape(symbol.rsplit(".", 1)[-1])
    return (rf"(^|[^A-Za-z0-9_])(def|class|function|func|fn|interface|type|struct|enum|const|let|var)[ \t]+{name}([^A-Za-z0-9_]|$)"
            rf"|^[ \t]*{name}[ \t]*[:=][^=]")


def serve_request(repo: Repo, request: Any, tests: "testing.TestSession | None" = None) -> tuple[str, str]:
    """Answer one investigator request from the head commit: (title, body). Raises Rejected for anything outside
    the closed set: five read-only lookups and, when the review may run tests, two sandboxed test executions."""
    if not isinstance(request, dict) or len(request) != 1:
        raise Rejected("a request is an object with exactly one of read, grep, list, definition, references, "
                       "symbol, callers, callees, tests_of, history, blame_range, pickaxe")
    kind, args = next(iter(request.items()))
    if not isinstance(args, dict):
        raise Rejected(f"{kind} takes an object")
    reference = getattr(repo, "reference", None)
    if kind in ("symbol", "callers", "callees", "tests_of"):
        name = args.get("name") if kind == "symbol" else args.get("symbol")
        if not isinstance(name, str) or not _SYMBOL.match(name.strip()):
            raise Rejected(f"{kind} takes one identifier in {'name' if kind == 'symbol' else 'symbol'}")
        answer = reference_module.serve(reference, kind, args) if reference is not None else None
        if answer is not None:
            return answer
        # The reference does not know the name: the text lookups answer instead (callees needs a definition).
        if kind == "callees":
            raise Rejected(f"no definition of {name.strip()} in the reference; ask for definition or grep instead")
        kind, args = ("definition" if kind == "symbol" else "references"), {"symbol": name.strip()}
    if kind == "read":
        path = repo.path(args.get("path"))
        lines = repo.lines(path)
        if lines is None:
            raise Rejected(f"{path} is binary or too large to read")
        try:
            start = max(1, int(args.get("start") or 1))
            end = int(args.get("end") or start + MAX_READ_LINES - 1)
        except (TypeError, ValueError):
            raise Rejected("read takes line numbers in start and end") from None
        end = min(end, len(lines), start + MAX_READ_LINES - 1)
        if start > len(lines):
            raise Rejected(f"{path} has {len(lines)} lines")
        return f"read {path}:{start}-{end} (of {len(lines)} lines)", _numbered(lines, start, end)
    if kind == "grep":
        pattern = args.get("pattern")
        if not isinstance(pattern, str) or not pattern or len(pattern) > MAX_PATTERN_CHARS or "\n" in pattern or "\0" in pattern:
            raise Rejected(f"grep takes a one-line pattern of at most {MAX_PATTERN_CHARS} characters")
        try:
            re.compile(pattern)
        except re.error as error:
            raise Rejected(f"bad regular expression: {error}") from None
        glob = args.get("path_glob")
        pathspec = None
        if glob is not None:
            if not isinstance(glob, str) or not _GLOB.match(glob) or glob.startswith(("/", "-", ":")) or ".." in glob.split("/"):
                raise Rejected("path_glob must be a relative glob inside the repository")
            pathspec = f":(glob){glob}"
        try:
            limit = min(MAX_GREP_HITS, max(1, int(args.get("max") or DEFAULT_GREP_HITS)))
        except (TypeError, ValueError):
            limit = DEFAULT_GREP_HITS
        # A line window scopes the hits to one construct; count_only reports the true count (up to a high cap)
        # instead of one cut at `max`. Either way `max` only bounds the hits shown.
        start, end = max(0, _int(args.get("start"), 0)), max(0, _int(args.get("end"), 0))
        scoped = start > 0 or end > 0
        count_only = args.get("count_only") is True
        fetch = MAX_COUNT_HITS if (scoped or count_only) else limit
        raw_hits = repo.grep(pattern, pathspec=pathspec, limit=fetch)
        hits = [hit for hit in raw_hits if (start or 1) <= hit[1] <= (end or 10 ** 9)] if scoped else raw_hits
        capped = len(raw_hits) >= fetch
        shown = hits[:limit]
        where = (f" in {glob}" if glob else "") + (f" lines {start or 1}-{end or 'end'}" if scoped else "")
        note = " (more not shown)" if capped else f" ({len(shown)} shown)" if len(shown) < len(hits) else ""
        return f"grep {pattern!r}{where} -> {len(hits)} matches{note}", _hits(shown)
    if kind == "list":
        raw = args.get("dir")
        if not isinstance(raw, str) or raw.startswith(("/", "~", "-", ":")) or ".." in raw.split("/") or "\0" in raw:
            raise Rejected("dir must be a directory inside the repository")
        directory = posixpath.normpath(raw.strip() or ".")
        prefix = "" if directory == "." else directory + "/"
        names: list[str] = []
        for path in repo.files():
            if not path.startswith(prefix):
                continue
            head, _, rest = path[len(prefix):].partition("/")
            entry = head + ("/" if rest else "")
            if entry not in names:
                names.append(entry)
        if not names:
            raise Rejected(f"{directory} is not a directory with tracked files")
        return f"list {directory} ({len(names)} entries)", "\n".join(names[:MAX_LIST]) + (
            f"\n... {len(names) - MAX_LIST} more" if len(names) > MAX_LIST else "")
    if kind in ("definition", "references"):
        symbol = args.get("symbol")
        if not isinstance(symbol, str) or not _SYMBOL.match(symbol):
            raise Rejected("symbol must be one identifier")
        if kind == "references":
            hits = repo.grep(symbol.rsplit(".", 1)[-1], fixed=True, word=True, limit=MAX_REFERENCES)
            body = _hits(hits)
            if reference is not None and reference.symbols_named(symbol):
                calls = reference.callers(symbol, outside_changes=False)
                if calls:
                    body = ("Call sites resolved by the reference:\n" + "\n".join(
                        f"{call.path}:{call.line}" + (f" in {call.enclosing}" if call.enclosing else "") + f": {_clip(call.text, 140)}"
                        for call in calls[:MAX_REFERENCES]) + "\n\nText matches:\n" + body)
            return (f"references {symbol} -> {len(hits)} matches"
                    + (" (more not shown)" if len(hits) >= MAX_REFERENCES else ""), body)
        hits = repo.grep(_definition_pattern(symbol), limit=6)
        parts = []
        if reference is not None:
            known = reference.symbols_named(symbol)
            if known:
                parts = [reference.symbol_view(item, body=True) for item in known[:2]]
                hits = [hit for hit in hits if not any(hit[0] == item.path and abs(hit[1] - item.line) <= 1 for item in known)]
        for path, line, _ in hits[:3]:
            lines = repo.lines(path)
            if lines:
                parts.append(f"{path}:\n{_numbered(lines, line, min(len(lines), line + 24))}")
        more = _hits(hits[3:]) if len(hits) > 3 else ""
        return f"definition {symbol} -> {len(hits)} matches", "\n\n".join(parts + ([more] if more else [])) or "0 matches"
    if kind == "history":
        path = repo.path(args.get("path"))
        count = min(MAX_HISTORY, max(1, _int(args.get("n"), 10)))
        lines = repo.history(path, count)
        return f"history {path} -> {len(lines)} commits", "\n".join(lines) or "no commits found"
    if kind == "blame_range":
        path = repo.path(args.get("path"))
        total = len(repo.lines(path) or [])
        start = max(1, _int(args.get("start"), 1))
        end = min(total, _int(args.get("end"), start), start + MAX_BLAME_LINES - 1)
        if start > total or end < start:
            raise Rejected(f"{path} has {total} lines")
        lines = repo.line_history(path, start, end)
        return f"blame_range {path}:{start}-{end} -> {len(lines)} commits", "\n".join(lines) or (
            "no history available for these lines")
    if kind == "pickaxe":
        text = args.get("string")
        if not isinstance(text, str) or not 3 <= len(text) <= 120 or "\n" in text or "\0" in text:
            raise Rejected("pickaxe takes a one-line string of 3 to 120 characters")
        lines = repo.pickaxe(text, min(MAX_HISTORY, max(1, _int(args.get("n"), 5))))
        return f"pickaxe {text!r} -> {len(lines)} commits", "\n".join(lines) or "no commit added or removed it"
    if kind in ("run_tests", "mutation_check"):
        if tests is None:
            raise Rejected("tests are not run in this review")
        try:
            if kind == "run_tests":
                record = tests.run(tests.paths(args.get("paths")), args.get("select"))
            else:
                record = tests.mutation(args.get("path"), args.get("line"), args.get("replacement"), args.get("tests"))
        except testing.TestsRejected as error:
            raise Rejected(str(error)) from None
        return f"test run {record['n']}", testing.summarize(record)
    raise Rejected(f"unknown request {kind!r}: use read, grep, list, definition, references, history, blame_range "
                   "or pickaxe")


def _int(value: Any, default: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def serve_blocks(repo: Repo, requests: Any, limit: int = ROUND_CHARS,
                 tests: "testing.TestSession | None" = None) -> tuple[list[tuple[str, str]], int, int]:
    """Answer a round of requests within the round's size limit: ([(title, block)], served, rejected)."""
    out: list[tuple[str, str]] = []
    size = served = rejected = 0
    items = requests if isinstance(requests, list) else []
    for request in items[:MAX_REQUESTS]:
        try:
            title, body = serve_request(repo, request, tests)
            served += 1
        except Rejected as error:
            title, body = f"rejected {json.dumps(request)[:160]}", str(error)
            rejected += 1
        block = f"## {title}\n{body}"
        if size + len(block) > limit:
            block = block[: max(0, limit - size)] + "\n... cut: the round's size limit is reached; ask for less at a time"
        out.append((title, block))
        size += len(block)
        if size >= limit:
            break
    if len(items) > MAX_REQUESTS:
        out.append(("not served", f"## not served\n{len(items) - MAX_REQUESTS} requests beyond the {MAX_REQUESTS} allowed per round"))
    return out, served, rejected


def serve(repo: Repo, requests: Any, limit: int = ROUND_CHARS,
          tests: "testing.TestSession | None" = None) -> tuple[str, int, int]:
    """Answer a round of requests within the round's size limit: (text, served, rejected)."""
    blocks, served, rejected = serve_blocks(repo, requests, limit, tests)
    return "\n\n".join(block for _title, block in blocks), served, rejected


# --- Map -------------------------------------------------------------------------------------------------------

_CONSTANT = re.compile(r"^\s*(?:export\s+)?(?:const\s+|final\s+|static\s+)*([A-Z][A-Z0-9_]{2,})\s*[:=]")
_ENV = re.compile(r"""(?:environ(?:\.get)?\s*[\[(]\s*|getenv\(\s*|process\.env\.|\$\{?)["']?([A-Z][A-Z0-9_]{3,})""")
_FLAG = re.compile(r"""["'\s](--[a-z][a-z0-9-]{2,})""")
_TABLE = re.compile(r"\b(?:alter|create)\s+table\s+(?:if\s+(?:not\s+)?exists\s+)?[`\"\[]?(\w+)"
                    r"|\badd\s+column\s+(?:if\s+not\s+exists\s+)?[`\"\[]?(\w+)", re.I)
_COMMENT = re.compile(r"^\s*(?:#+|//+|/?\*+|--|<!--|\"\"\"|''')\s?(.{15,})$")
#: A changed code line that is behaviour: not blank, not a comment, not a bare docstring or string line.
_BEHAVIOUR = re.compile(r'^\s*(?!#|//|/\*|\*|--|<!--|-->)(?!"""[^"]*("""|$))(?!\'\'\'[^\']*(\'\'\'|$))(?!["\'][^"\']*["\']\s*,?\s*$)\S')

_DESCRIPTION = re.compile(r"""\b(?:description|help|doc|summary)\s*[:=]\s*["'](.{15,})["']""")
_DOCLIKE = re.compile(r"\.(md|mdx|rst|txt|adoc|ya?ml|toml|cfg|ini|json|sh|tf|env)$|(^|/)(\.github|ci|scripts|docs?)/|(^|/)Dockerfile", re.I)
_STRUCTURE = r"parametrize|fixture|it\.each|test\.each|describe\(|@pytest\.mark"
#: Files whose lines are values the author chose (configuration, manifests, workflows, infrastructure, SQL, shell
#: scripts, documents): no test "pins" a literal there, so a missing-test finding about one is not a finding.
_NON_CODE = re.compile(r"\.(ya?ml|json|toml|ini|cfg|env|tf|tfvars|hcl|sql|sh|bash|zsh|properties|xml|txt|csv|lock|md|mdx|rst|adoc)$"
                       r"|(^|/)(Dockerfile|Makefile|\.env[^/]*)$", re.I)
_CONFIG_PATH = re.compile(r"\.(ya?ml|toml|cfg|ini|tf|env|properties)$|(^|/)(\.github|ci|deploy|k8s|helm|charts|infra|terraform)/"
                          r"|(^|/)(Dockerfile|docker-compose[^/]*|Makefile|package\.json|requirements[^/]*\.txt|pyproject\.toml"
                          r"|go\.mod|Cargo\.toml|Gemfile|pom\.xml|build\.gradle[^/]*)$", re.I)
_INPUT_HINT = re.compile(r"\bre\.|regex|RegExp|\.match\(|\.test\(|\.search\(|parse|split\(|strip\(|startswith|endswith"
                         r"|validate|sanitiz|escape|limit|max_|min_|len\(|\.length|truncate|slice\(|\bint\(|float\("
                         r"|isinstance|typeof|\bif .*(<|>|<=|>=|==|!=)", re.I)
_REQUEST_INPUT = re.compile(r"""\b(?:request|req|ctx)\.(?:args|form|json|GET|POST|params|query|body|headers|data)"""
                            r"""(?:\.get\(\s*|\[\s*|\.)["']?([A-Za-z_][A-Za-z0-9_]{2,})""")
_CONFIG_KEY = re.compile(r"""^\s*-?\s*["']?([A-Za-z_][A-Za-z0-9_.-]{2,})["']?\s*[:=]""")
#: A field or key declared in code (`key: 'metrics_ingress_cidrs'`, `name: "x"`): a member of a family whose
#: siblings and consumers must be checked.
_FIELD = re.compile(r"""^\s*\{?\s*(?:key|name|field|flag|option)\s*[:=]\s*["']([A-Za-z_][A-Za-z0-9_.-]{2,})["']""")
_CONFIG_FILE = re.compile(r"\.(ya?ml|toml|ini|cfg|env|properties|json|tf)$|(^|/)\.env", re.I)
_WORD = re.compile(r"`([^`]{2,60})`|\b([A-Za-z_][A-Za-z0-9_]{3,})\b")
_RISK = re.compile(r"auth|token|password|passwd|secret|credential|permission|role|session|cookie|sql|query\(|execute\("
                   r"|eval\(|exec\(|subprocess|os\.system|shell|pickle|yaml\.load|request\.|route|endpoint|router\."
                   r"|@app\.|email|phone|ssn|pii|encrypt|decrypt|hash|cors|csrf|upload|redirect", re.I)


@dataclass
class Brief:
    text: str
    lenses: list[str]
    symbols: list[str] = field(default_factory=list)
    callers: int = 0
    tests: list[str] = field(default_factory=list)
    claims: int = 0
    #: Names an investigator must look up before it may finish, by part: the changed signatures and exported
    #: names (siblings), and the new config fields, flags, environment variables and request inputs (claims).
    required: dict[str, list[str]] = field(default_factory=dict)
    #: The claims as listed in the brief: (source, text).
    claim_list: list[tuple[str, str]] = field(default_factory=list)
    helpers: list[str] = field(default_factory=list)
    extracted: dict[str, list[Any]] = field(default_factory=dict)
    #: The structural reference (definitions resolved, callers, tests, workflow and Terraform facts), its view
    #: for the frames, and its stats; None and "" when it could not be built.
    reference: Any = None
    reference_text: str = ""
    reference_stats: dict[str, Any] = field(default_factory=dict)


def _add(items: list[Any], item: Any, limit: int) -> None:
    if item not in items and len(items) < limit:
        items.append(item)


def extract(files: list[FileDiff], read_file: Callable[[str], list[str] | None]) -> dict[str, list[Any]]:
    """What the diff defines, changes, calls and claims: names worth following into the repository."""
    out: dict[str, list[Any]] = {"defined": [], "changed": [], "calls": [], "constants": [], "env": [], "flags": [],
                                 "tables": [], "claims": [], "inputs": [], "config": [], "fields": []}
    for item in files:
        if skip_reason(item):
            continue
        doc = file_kind(item.path) == "doc"
        source = read_file(item.path)
        for hunk in item.hunks:
            added = [line for line in hunk.lines if line.kind == "+"]
            # The definition an added line sits in changed, even when its `def` line did not.
            if source and not doc:
                for line in added:
                    first = line.new or hunk.new_first
                    for index in range(min(first, len(source)) - 1, max(-1, first - 80), -1):
                        match = _DEF.match(source[index])
                        if match:
                            _add(out["changed"], match.group(1) or match.group(2), 12)
                            break
            for line in added:
                text = line.text
                if _CONFIG_FILE.search(item.path):
                    key = _CONFIG_KEY.match(text)
                    if key and not text.lstrip().startswith(("#", "//")):
                        _add(out["config"], key.group(1), 8)
                if doc:
                    if len(text.strip()) >= 20:
                        _add(out["claims"], (item.path, line.new, _clip(text.strip(), 200)), 14)
                    for flag in _FLAG.findall(" " + text):
                        _add(out["flags"], flag, 6)
                    continue
                match = _DEF.match(text)
                if match:
                    _add(out["defined"], match.group(1) or match.group(2), 12)
                match = _CONSTANT.match(text)
                if match:
                    _add(out["constants"], match.group(1), 10)
                match = _FIELD.match(text)
                if match:
                    _add(out["fields"], match.group(1), 8)
                for name in _CALL.findall(text):
                    if name not in _COMMON:
                        _add(out["calls"], name, 16)
                for name in _ENV.findall(text):
                    _add(out["env"], name, 6)
                for name in _REQUEST_INPUT.findall(text):
                    _add(out["inputs"], name, 6)
                for flag in _FLAG.findall(" " + text):
                    _add(out["flags"], flag, 6)
                for pair in _TABLE.findall(text):
                    _add(out["tables"], pair[0] or pair[1], 6)
                claim = _COMMENT.match(text) or _DESCRIPTION.search(text)
                if claim:
                    _add(out["claims"], (item.path, line.new, _clip(claim.group(1).strip(), 200)), 14)
    own = set(out["defined"]) | set(out["changed"])
    out["calls"] = [name for name in out["calls"] if name not in own][:8]
    return out


def build_brief(repo: Repo, files: list[FileDiff], read_file: Callable[[str], list[str] | None],
                limit: int = BRIEF_CHARS, *, title: str = "", description: str = "",
                base: str | None = None) -> Brief:
    """The investigation brief: what the change claims, where each claim must hold, and where the diff's names
    live in the rest of the repository."""
    found = extract(files, read_file)
    # The structural reference: the files themselves read (ast for Python, a scanner for TS/JS, readers for
    # workflows and Terraform), so "who calls this" is resolved through imports rather than grepped. A failure
    # leaves the text search below in place.
    reference: Any = None
    reference_error = ""
    try:
        base_repo = Repo(repo.root, base, repo._runner) if base else None
        reference = reference_module.Reference(repo, files, base_repo=base_repo).build()
    except Exception as error:
        reference = None
        repo.reference = None
        reference_error = f"{type(error).__name__}: {error}"
    changed_lines: dict[str, set[int]] = {}
    for item in files:
        changed_lines[item.path] = {line.new for hunk in item.hunks for line in hunk.lines
                                    if line.kind == "+" and line.new is not None}
    reviewable = [item for item in files if not skip_reason(item)]
    code_files = [item.path for item in reviewable if file_kind(item.path) == "code"]
    test_files = [item.path for item in reviewable if file_kind(item.path) == "test"]
    sections: list[str] = []
    callers = 0
    tests: list[str] = []
    #: Where each name is used outside the changed lines, for the claims' obligations.
    used_at: dict[str, list[str]] = {}

    def uses(names: list[str], title: str, per: int = 8) -> None:
        nonlocal callers
        for name in names:
            hits = [hit for hit in repo.grep(name, fixed=True, word=True, limit=per + 12)
                    if hit[1] not in changed_lines.get(hit[0], ())]
            resolved = ""
            if reference is not None and reference.symbols_named(name):
                # Call sites resolved through imports first; text hits only from files the reference did not read.
                calls = [(call.path, call.line, call.text) for call in reference.callers(name)]
                if calls:
                    hits = calls + [hit for hit in hits if hit[0] not in reference.index]
                    resolved = " (callers resolved)"
            in_tests = [hit for hit in hits if _TEST.search(hit[0])]
            in_docs = [hit for hit in hits if _DOCLIKE.search(hit[0]) and hit not in in_tests]
            others = [hit for hit in hits if hit not in in_tests and hit not in in_docs]
            callers += len(others)
            used_at[name] = [f"{p}:{n}" for p, n, _ in (others + in_tests)[:5]]
            for hit in in_tests:
                _add(tests, hit[0], 6)
            lines = [f"{title} `{name}`{resolved}:"]
            lines.append("  used at: " + ("; ".join(f"{p}:{n}: {_clip(t.strip(), 110)}" for p, n, t in others[:per]) or "nowhere else"))
            lines.append("  tests: " + ("; ".join(f"{p}:{n}: {_clip(t.strip(), 110)}" for p, n, t in in_tests[:5])
                                        or "no test mentions it"))
            if in_docs:
                lines.append("  docs and configs: " + "; ".join(f"{p}:{n}" for p, n, _ in in_docs[:6]))
            sections.append("\n".join(lines))

    symbols = list(dict.fromkeys(found["defined"] + found["changed"]))[:8]
    uses(symbols, "Changed or added")
    uses(found["constants"][:5], "Constant or member")
    uses((found["env"] + found["flags"] + found["tables"])[:6], "Named in the change")
    helpers: list[str] = []
    for name in found["calls"][:5]:
        hits = repo.grep(_definition_pattern(name), limit=3)
        if not hits:
            continue
        path, line, _ = hits[0]
        lines = repo.lines(path)
        if not lines:
            continue
        helpers.append(name)
        sections.append(f"Called by the change, `{name}` is defined at {path}:{line}:\n"
                        + _numbered(lines, line, min(len(lines), line + 11)))
    for path in list(dict.fromkeys(tests + test_files))[:4]:
        hits = repo.grep(_STRUCTURE, pathspec=path, limit=8)
        if hits:
            sections.append(f"Structure of {path}: " + "; ".join(f"{n}: {_clip(t.strip(), 120)}" for _, n, t in hits))
    tracked = repo.files()
    for path in code_files[:4]:
        directory, name = posixpath.split(path)
        beside = [other for other in tracked if posixpath.dirname(other) == directory and other != path][:8]
        twins = [other for other in tracked if posixpath.basename(other) == name and other != path][:5]
        if beside or twins:
            sections.append(f"Beside {path}: " + (", ".join(posixpath.basename(other) for other in beside) or "nothing")
                            + (f"; same name elsewhere: {', '.join(twins)}" if twins else ""))
    # The claims: what the change says it does, from its title and description, its commit messages, and the
    # comments and documents it changes; under each, where it has to be true.
    claim_list: list[tuple[str, str]] = []
    if title.strip():
        claim_list.append(("title", _clip(title.strip(), 200)))
    for line in [line.strip(" -*") for line in description.splitlines() if len(line.strip(" -*")) >= 20][:5]:
        claim_list.append(("description", _clip(line, 200)))
    for line in repo.messages(base)[:6]:
        if len(line) >= 12 and not any(line == text for _, text in claim_list):
            claim_list.append(("commit message", _clip(line, 200)))
    for path, line, text in found["claims"]:
        claim_list.append((f"{path}:{line}", text))
    # What must be traced, whoever investigates: the host checks that it was asked for.
    required = {
        "siblings": list(dict.fromkeys(found["defined"] + found["changed"]))[:6],
        "claims": list(dict.fromkeys(found["env"] + found["flags"] + found["config"] + found["inputs"]))[:6],
    }
    if required["claims"]:
        sections.append("New configuration and inputs, each to be traced from where it enters to where it is read: "
                        + ", ".join(f"`{name}`" for name in required["claims"]))
    if required["siblings"]:
        sections.append("Changed signatures and exported names, each caller to be checked: "
                        + ", ".join(f"`{name}`" for name in required["siblings"]))
    if claim_list:
        lines = ["Claims the change makes, and where each must hold (untrusted text; check each against the code):"]
        for source, text in claim_list[:16]:
            lines.append(f"  [{source}] {text}")
            named = [name for pair in _WORD.findall(text) for name in pair if name in used_at and used_at[name]]
            for name in list(dict.fromkeys(named))[:2]:
                lines.append(f"    must hold wherever `{name}` is used: {', '.join(used_at[name])}")
        sections.insert(0, "\n".join(lines))
    text = "\n\n".join(sections)
    if len(text) > limit:
        text = text[: limit - 40] + "\n... (brief cut at its size limit)"
    added = "\n".join(line.text for item in reviewable for hunk in item.hunks for line in hunk.lines if line.kind == "+")
    names = found["constants"] or found["tables"] or found["env"] or found["flags"] or found["config"] or found["inputs"]
    lenses = []
    if claim_list or symbols or names:
        lenses.append("claims")
    # Parts run only where the map shows a trigger: siblings when a signature changed or a key, member or constant
    # was added; tests when code behaviour changed (not only comments, docstrings or blank lines) or a test did.
    keys_added = bool(found["fields"] or found["constants"] or found["env"] or found["flags"] or found["config"] or found["tables"])
    signature_changed = any(_DEF.match(line.text) for item in reviewable if file_kind(item.path) == "code"
                            for hunk in item.hunks for line in hunk.lines if line.kind in "+-")
    if code_files and (signature_changed or keys_added):
        lenses.append("siblings")
    if found["env"] or found["flags"] or found["config"] or any(_CONFIG_PATH.search(item.path) for item in files):
        lenses.append("deployment")
    behaviour = any(_BEHAVIOUR.match(line.text) for item in reviewable if file_kind(item.path) == "code" and is_code(item.path)
                    for hunk in item.hunks for line in hunk.lines if line.kind in "+-")
    if (code_files and behaviour) or test_files:
        lenses.append("tests")
    if code_files and (_INPUT_HINT.search(added) or _RISK.search(added)):
        lenses.append("inputs")
    brief = Brief(text or "(nothing in the repository mentions the changed names)", lenses, symbols, callers, tests,
                  len(claim_list), required, claim_list, helpers, found)
    if reference is not None:
        try:
            brief.reference = reference
            brief.reference_text = reference.block()
            brief.reference_stats = dict(reference.stats)
        except Exception as error:
            brief.reference_text = ""
            reference_error = f"{type(error).__name__}: {error}"
    if reference_error:
        brief.reference_stats = {"error": _text(reference_error, 200)}  # reported under timing.reference
    return brief


# --- Evidence --------------------------------------------------------------------------------------------------


def _flat(text: str) -> str:
    return " ".join(text.split())


def check_citations(repo: Repo, raw: Any, diff_lines: dict[str, set[int]]) -> tuple[list[dict[str, Any]], str | None, bool]:
    """A finding's citations checked against the head commit: (citations, why it fails or None, whether any
    citation is outside the diff). A quote must be on its cited line, give or take two lines."""
    items = raw if isinstance(raw, list) else []
    citations: list[dict[str, Any]] = []
    beyond = False
    for item in items[:MAX_CITATIONS]:
        if not isinstance(item, dict):
            return [], "a citation is not an object", False
        try:
            path = repo.path(item.get("path"))
            line = int(item.get("line"))
        except (Rejected, TypeError, ValueError) as error:
            return [], f"a citation names no tracked file and line ({error})", False
        quote = _flat(item.get("quote") if isinstance(item.get("quote"), str) else "")
        lines = repo.lines(path)
        if len(quote) < 4 or not lines:
            return [], f"the citation of {path}:{line} quotes nothing", False
        low, high = max(1, line - CITATION_SLACK), min(len(lines), line + CITATION_SLACK)
        at = next((number for number in range(low, high + 1) if quote in _flat(lines[number - 1])
                   or (len(_flat(lines[number - 1])) >= 8 and _flat(lines[number - 1]) in quote)), None)
        if at is None:
            return [], f"{path}:{line} does not say {_clip(quote, 80)!r}", False
        citations.append({"path": path, "line": at, "quote": _clip(lines[at - 1].strip(), 240)})
        if at not in diff_lines.get(path, ()):
            beyond = True
    if not citations:
        return [], "no evidence", False
    return citations, None, beyond


def normalize_deep(repo: Repo, raw: Any, lens: str, diff_lines: dict[str, set[int]],
                   cap: Callable[[str, str, str], str], runs: list[dict[str, Any]] | None = None,
                   to_level: Callable[[Any], str | None] | None = None,
                   enrich: Callable[[Any, dict[str, Any]], None] | None = None) -> tuple[dict[str, Any] | None, str | None]:
    """One investigator finding, validated: (finding, None) or (None, why it was dropped)."""
    if not isinstance(raw, dict):
        return None, "not an object"
    claim = _text(raw.get("claim"), 300)
    if not claim:
        return None, "no claim"
    try:
        path = repo.path(raw.get("file"))
        line = int(raw.get("line"))
    except (Rejected, TypeError, ValueError) as error:
        return None, f"no tracked file and line ({error})"
    lines = repo.lines(path)
    if not lines or not 1 <= line <= len(lines):
        return None, f"{path} has no line {line}"
    citations, problem, beyond = check_citations(repo, raw.get("evidence"), diff_lines)
    # A test run the host made can stand in for quoted lines; a run that could not run proves nothing.
    number = raw.get("test_run")
    run = next((item for item in runs or [] if item["n"] == number and item["status"] != "unavailable"), None) \
        if isinstance(number, int) and not isinstance(number, bool) else None
    if problem and (run is None or problem != "no evidence"):
        return None, problem
    if run is not None:
        beyond = True
    name = str(raw.get("severity", "")).strip().lower()
    stated = (to_level(name) if to_level else name if name in LEVEL_ORDER else None) or "low"
    category = normalize_category(_text(raw.get("category"), 40), {"tests": "tests"}.get(lens, "correctness"))
    scenario = _text(raw.get("scenario"), 500)
    try:
        confidence = min(1.0, max(0.0, float(raw.get("confidence", 0.6))))
    except (TypeError, ValueError):
        confidence = 0.6
    finding = {
        "file": path, "line": line, "level": (level := cap(stated, category, scenario)),
        "severity": LEVEL_TO_OLD[level], "finder_level": stated, "category": category, "claim": claim, "why": _text(raw.get("why"), 600), "scenario": scenario,
        "suggested_fix": _text(raw.get("suggested_fix"), 500), "confidence": confidence,
        "reviewers": [f"deep:{lens}"], "source": f"deep:{lens}", "citations": citations, "beyond_diff": beyond,
        **({"test_run": run["n"], "test_evidence": testing.summarize(run).split("\n  output")[0]} if run else {}),
    }
    if enrich is not None:
        enrich(raw, finding)
    return finding, None


def cited_windows(repo: Repo, finding: dict[str, Any]) -> str:
    """The source around each citation of a finding, as the host reads it at the head commit."""
    parts = []
    for citation in finding.get("citations") or []:
        lines = repo.lines(citation["path"])
        if lines:
            start, end = max(1, citation["line"] - CITED_WINDOW), min(len(lines), citation["line"] + CITED_WINDOW)
            parts.append(f"{citation['path']} (cited line {citation['line']}):\n{_numbered(lines, start, end)}")
    return "\n\n".join(parts)


def _nearly_identical(a: str, b: str) -> bool:
    left = {word for word in re.findall(r"[a-z0-9_]+", a.lower()) if len(word) > 2}
    right = {word for word in re.findall(r"[a-z0-9_]+", b.lower()) if len(word) > 2}
    return len(left) >= 3 and len(right) >= 3 and len(left & right) / min(len(left), len(right)) >= 0.8


def _record(records: list[dict[str, Any]] | None, dropped: dict[str, Any], into: dict[str, Any], stage: str) -> None:
    if records is None:
        return
    brief = lambda item: {"file": item["file"], "line": item["line"], "category": item.get("category"),  # noqa: E731
                          "claim": _text(item.get("claim"), 160), "source": item.get("source") or "fast"}
    records.append({"dropped": brief(dropped), "into": brief(into), "stage": stage})


def merge(fast: list[dict[str, Any]], deep: list[dict[str, Any]],
          records: list[dict[str, Any]] | None = None) -> tuple[list[dict[str, Any]], int]:
    """Deep findings deduplicated across investigators, then against the fast pass. Two findings are one when
    they are of one category and either sit in one file within `MERGE_WINDOW` lines of each other and come from
    different passes, or make nearly identical claims. Findings of different categories never merge. A deep
    finding with evidence from outside the diff supersedes the fast one it extends and is never folded into it;
    a deep finding whose evidence is all inside the diff adds nothing to a fast one and gives way to it.
    Returns (all, duplicates)."""
    def same(a: dict[str, Any], b: dict[str, Any]) -> bool:
        if a["category"] != b["category"]:
            return False
        if _nearly_identical(a["claim"], b["claim"]) and a["file"] == b["file"]:
            return True
        # One investigator's two findings on neighbouring lines are two findings.
        return (a["file"] == b["file"] and abs(a["line"] - b["line"]) <= MERGE_WINDOW
                and a.get("source") != b.get("source"))

    order = {level: index for index, level in enumerate(LEVEL_ORDER)}
    kept: list[dict[str, Any]] = []
    duplicates = 0
    for finding in sorted(deep, key=lambda item: (order[item["level"]], -len(item.get("citations") or []))):
        twin = next((item for item in kept if same(item, finding)), None)
        if twin is None:
            kept.append(finding)
            continue
        duplicates += 1
        _record(records, finding, twin, "deep")
        twin["reviewers"] += [name for name in finding["reviewers"] if name not in twin["reviewers"]]
    for finding in [item for item in kept if not item.get("beyond_diff")]:
        other = next((item for item in fast if same(finding, item)), None)
        if other is not None:
            kept.remove(finding)
            duplicates += 1
            _record(records, finding, other, "deep-into-fast")
    remaining = []
    for finding in fast:
        twin = next((item for item in kept if same(item, finding)), None)
        if twin is None:
            remaining.append(finding)
            continue
        duplicates += 1
        _record(records, finding, twin, "fast-into-deep")
        twin["reviewers"] += [name for name in finding.get("reviewers") or [] if name not in twin["reviewers"]]
        if not twin.get("replacement") and finding.get("replacement") and twin["line"] == finding["line"]:
            twin["replacement"] = finding["replacement"]
            if finding.get("end_line"):
                twin["end_line"] = finding["end_line"]
    return remaining + kept, duplicates


# --- Investigators ---------------------------------------------------------------------------------------------

LAST_ROUND = ("This is your last round: requests will not be served. Reply with your final findings, "
              "\"requests\": [] and \"done\": true.")


#: Which part owes each catalogue check shape (see review_prompts.CHECK_CATALOGUE); a shape whose part does not
#: run on a change falls to the first part that does.
SHAPE_LENS = {"registry-member": "siblings", "unpinned-behaviour": "tests", "test-asserts-behaviour": "tests",
              "env-in-deploy": "deployment", "manifest-reference": "deployment", "workflow-siblings": "deployment",
              "input-defeats-guard": "inputs", "guard-after-effect": "siblings", "error-path": "siblings",
              "comment-vs-code": "claims", "sibling-implementation": "siblings", "failure-retry": "siblings"}


def shapes_by_lens(shapes: list[dict[str, str]], lenses: list[str]) -> dict[str, list[dict[str, str]]]:
    """The catalogue checks each running part owes."""
    out: dict[str, list[dict[str, str]]] = {lens: [] for lens in lenses}
    for item in shapes:
        lens = SHAPE_LENS.get(item.get("name", ""))
        if lens not in out:
            lens = lenses[0] if lenses else None
        if lens is not None:
            out[lens].append(item)
    return out


def _mentions(text: str, item: dict[str, str]) -> bool:
    return bool(re.search(r"(?<![A-Za-z0-9])" + re.escape(item["id"]) + r"(?![0-9])", text)) or item.get("name", "") in text


async def investigate(frames: Any, lens: str, base_views: list[str], repo: Repo, *, rounds: int, model: str | None,
                      thinking: str | None, cutoff: float | None, clock: Callable[[], float],
                      tests: "testing.TestSession | None" = None, something_outside: bool = True,
                      required: list[str] | None = None, shapes: list[dict[str, str]] | None = None,
                      shared: list[str] | None = None, leads_view: "asyncio.Future[str | None] | None" = None) -> dict[str, Any]:
    """One investigator's retrieval loop: ask the frame, serve what it requests, ask again. `shapes` are the
    catalogue checks this part owes (`T<n>` items): each must be made (named in a finding or in checked) or
    declared not applicable in checked, else the investigator is sent back once. `shared` are the views every
    frame of the review starts with (the diff, the brief, the retrieved block, the intent, the guides): sent as
    the frames' shared prefix, so a provider's prompt cache can serve it. After the first round a frame gets the
    new results in full and a one-line ledger of the earlier ones (ids), which `{"recall": {"id": "r1.2"}}`
    re-reads in full. `leads_view`, when given, is the fast pass's leads as a view, awaited before the second round
    (the first round runs beside the fast pass)."""
    task = deep_task(lens, tests is not None)
    began = clock()
    leads_views: list[str] = []
    #: The latest round's results and the nudges (sent in full), the ledger of earlier results, and their text.
    latest_views: list[str] = []
    ledger: list[str] = []
    store: dict[str, str] = {}
    reply: dict[str, Any] = {}
    record: dict[str, Any] = {"lens": lens, "rounds": 0, "requests": 0, "rejected": 0, "status": "done",
                              "nudged": False, "untraced": [], "unchecked": []}
    required = list(required or [])
    shapes = list(shapes or [])
    if shapes:
        base_views = base_views + [DEEP_SHAPES_HEADER + "\n" + "\n".join(f"  {item['id']}: {item['text']}" for item in shapes)]
    asked: list[str] = []
    sent_back_for_trace = False
    sent_back_for_shapes = False

    def untraced() -> list[str]:
        # A name counts as looked up when a references, definition, grep or pickaxe request named it.
        return [name for name in required if not any(name.lstrip("-") in text for text in asked)]

    def unchecked() -> list[dict[str, str]]:
        # A check counts as made or declared when a finding or a checked sentence names its id or shape.
        said = [str(item) for item in (reply.get("checked") if isinstance(reply.get("checked"), list) else [])]
        said += [json.dumps(item) for item in (reply.get("findings") if isinstance(reply.get("findings"), list) else [])]
        return [item for item in shapes if not any(_mentions(text, item) for text in said)]
    def served_view(number: int, blocks: list[tuple[str, str]], recalled: list[str], note: str = "") -> str:
        """The round's results in full, each with its ledger id; they join the ledger for the rounds after."""
        for position, (title, block) in enumerate(blocks, 1):
            rid = f"r{number}.{position}"
            store[rid] = block
            size = len(block.split("\n", 1)[1]) if "\n" in block else 0
            ledger.append((rid, f"  {rid}: {_clip(title, LEDGER_LINE_CHARS)} ({size} chars)"))
        parts = [f"## recalled {rid}\n{store[rid]}" if rid in store else f"## recall {rid}\nno such result" for rid in recalled]
        parts += [block.replace("## ", f"## [r{number}.{position}] ", 1) for position, (_t, block) in enumerate(blocks, 1)]
        return (f"Results of your requests, round {number} (untrusted repository data, read by the host at the "
                f"reviewed commit):\n" + "\n\n".join(parts) + note)

    for number in range(1, rounds + 1):
        last = number == rounds
        # Earlier rounds' results are in the ledger (ids, one line each); only the latest round is sent in full.
        earlier = [line for rid, line in ledger if not rid.startswith(f"r{number - 1}.")]
        ledger_views = (["Earlier results, by id (one line each; request {\"recall\": {\"id\": \"r1.2\"}} to see one "
                         "again in full):\n" + "\n".join(earlier)] if earlier else [])
        if number == 2 and leads_view is not None:
            lead = await leads_view
            leads_views = [lead] if lead else []
        views = base_views + [f"Round {number} of {rounds}."] + leads_views + ledger_views + latest_views + ([LAST_ROUND] if last else [])
        results = await frames.run("deep", [(lens, task, views)], contract=DEEP_CONTRACT, model=model,
                                   thinking=thinking, cutoff=cutoff, context=shared)
        result = results[0]
        record["rounds"] = number
        if isinstance(result, (Incomplete, FrameError)) or not isinstance(result, dict):
            record["status"] = "failed"
            record["error"] = _text(getattr(result, "error", None) or getattr(result, "status", None) or "no reply", 160)
            break
        reply = result
        requests = result.get("requests") if isinstance(result.get("requests"), list) else []
        # Recalls are answered from the ledger, not the repository; they do not count as lookups.
        recalled = [str(request["recall"].get("id")) for request in requests if isinstance(request, dict)
                    and isinstance(request.get("recall"), dict)]
        requests = [request for request in requests if not (isinstance(request, dict) and "recall" in request)]
        for request in requests[:MAX_REQUESTS]:
            if isinstance(request, dict):
                for args in request.values():
                    if isinstance(args, dict):
                        asked += [str(args[key]) for key in ("symbol", "pattern", "string") if isinstance(args.get(key), str)]
        if result.get("done") is True or not (requests or recalled):
            # Finishing is refused, once for each reason, when the investigator has not done its part: next to
            # nothing looked up in the first round although the map shows there is something outside the diff,
            # or names its part must trace that it never asked for.
            reasons = []
            if number == 1 and not record["nudged"] and something_outside and record["requests"] + len(requests) < MIN_LOOKUPS:
                record["nudged"] = True
                reasons.append(DEEP_NUDGE)
            missing = untraced()
            if missing and not sent_back_for_trace:
                sent_back_for_trace = True
                reasons.append(DEEP_TRACE_NUDGE + ", ".join(missing) + ".")
            owed = unchecked()
            if owed and not sent_back_for_shapes:
                sent_back_for_shapes = True
                reasons.append(DEEP_SHAPE_NUDGE + ", ".join(f"{item['id']} ({item['name']})" for item in owed) + ".")
            if reasons and not last:
                blocks, served, rejected = serve_blocks(repo, requests, tests=tests) if requests else ([], 0, 0)
                record["requests"] += served
                record["rejected"] += rejected
                view = served_view(number, blocks, recalled) if blocks or recalled else None
                latest_views[:] = ([view] if view else []) + ["\n\n".join(reasons)]
                continue
            break
        if last:
            record["status"] = "rounds exhausted"
            break
        blocks, served, rejected = serve_blocks(repo, requests, tests=tests)
        note = f"\n\nTest executions left in this review: {max(0, tests.limit - len(tests.records))}." if tests is not None else ""
        record["requests"] += served
        record["rejected"] += rejected
        latest_views[:] = [served_view(number, blocks, recalled, note)]
    record["untraced"] = untraced() if record["status"] != "failed" else []
    record["unchecked"] = [f"{item['id']} {item['name']}" for item in unchecked()] if record["status"] != "failed" else []
    record["ms"] = int((clock() - began) * 1000)
    record["tokens"] = sum(int(item.get("tokens") or 0) for item in frames.timings
                           if item.get("phase") == "deep" and item.get("reviewer") == lens)
    return {"lens": lens, "reply": reply, "record": record}


def _test_line(repo: Repo, test_id: str) -> tuple[str | None, int]:
    """The file and line of a test from its id (`path::name`, or a path)."""
    path, _, name = test_id.partition("::")
    try:
        path = repo.path(path)
    except Rejected:
        return None, 1
    leaf = re.split(r"[\[:]", name.split("::")[-1])[0] if name else ""
    for number, line in enumerate(repo.lines(path) or [], 1):
        if leaf and re.search(rf"\b{re.escape(leaf)}\b", line):
            return path, number
    return path, 1


def prove_unpinned(session: "testing.TestSession | None", repo: Repo, findings: list[dict[str, Any]],
                   brief: Brief) -> None:
    """Settle tests findings by running them: for each that names its unpinned change as a one-line replacement
    (most serious first, while test executions are left), the host applies the replacement, runs the nearest
    tests, and marks the finding `proof`: "proven" when the suite still passes (nothing pins that line: the run is
    its evidence), "refuted" when a test fails (an existing test does catch it), or leaves it to reasoning when
    the tests could not run or were already failing."""
    if session is None:
        return
    order = {level: index for index, level in enumerate(LEVEL_ORDER)}
    candidates = [item for item in findings if item.get("category") == "tests" and (item.get("unpinned") or {}).get("mutation")
                  and not item.get("proof")]
    for finding in sorted(candidates, key=lambda item: order.get(item.get("level", "low"), 3)):
        if len(session.records) >= session.limit:
            break
        unpinned = finding["unpinned"]
        closest = (unpinned.get("closest_test") or {}).get("path")
        paths = [closest] if closest in session.tracked and _TEST.search(closest or "") else brief.tests[:2]
        if not paths:
            continue
        # A suite that already fails says nothing about the mutant.
        already = any(record["kind"] == "automatic" and record["status"] != "passed" and set(paths) & set(record["paths"])
                      for record in session.records)
        try:
            record = session.mutation(unpinned["mutation"]["path"], unpinned["mutation"]["line"],
                                      unpinned["mutation"]["replacement"], paths)
        except testing.TestsRejected:
            continue
        if record["status"] == "passed":
            finding.update(proof="proven", test_run=record["n"], beyond_diff=True,
                           test_evidence=testing.summarize(record).split("\n  output")[0])
        elif record["status"] == "failed" and not already:
            finding["proof"] = "refuted"


def regression_findings(repo: Repo, compared: dict[str, Any]) -> list[dict[str, Any]]:
    """A test that fails at the reviewed commit and passed at the base commit is a finding by itself: the host ran
    both, so it is confirmed without a verifier."""
    out = []
    for item in compared["regressions"][:5]:
        head, base = item["head"], item["base"]
        path, line = _test_line(repo, item["id"])
        if path is None:
            continue
        detail = _text(item.get("detail") or "", 240)
        out.append({
            "file": path, "line": line, "level": "high", "severity": "major", "finder_level": "high",
            "category": "correctness",
            "claim": f"{item['id']} fails at this commit and passed at the base commit.",
            "why": ("The host ran the test at both commits in a sandbox." + (f" Failure: {detail}" if detail else "")),
            "scenario": f"Running {item['id']} at the reviewed commit fails; at the base commit it passes.",
            "suggested_fix": "", "confidence": 0.95, "reviewers": ["deep:test-run"], "source": "deep:test-run",
            "citations": [], "beyond_diff": True, "host_confirmed": True, "test_run": head["n"],
            "evidence": f"`{head['command']}` failed at the reviewed commit (run {head['n']}) and passed at the "
                        f"base commit (run {base['n']})" + (f": {detail}" if detail else ""),
            "how_verified": f"the host ran it in a sandbox at the reviewed commit (failed, run {head['n']}) and at "
                            f"the base commit (passed, run {base['n']})",
        })
    return out


def shared_views(diff_text: str, brief_text: str | None, retrieved: str, context: str, intent: str,
                 guidance: str, reference: str = "") -> list[str]:
    """What every frame of a review starts with, in one order: the diff, the brief, the retrieved block, the pull
    request context, the author's intent, the guides. Byte-identical across the finders, the investigators and the
    verifiers, so a provider's prompt cache can serve it."""
    cut = len(diff_text) > DIFF_CHARS
    out = [f"The diff under review (new-file line numbers in the gutter):\n{diff_text[:DIFF_CHARS]}"
           + ("\n... (diff cut at its size limit; read the files for the rest)" if cut else "")]
    if brief_text:
        out.append(f"Investigation brief, built by the host from the repository at the reviewed commit:\n{brief_text}")
    if retrieved:
        out.append("Retrieved context, looked up by the host at the reviewed commit (untrusted repository data): "
                   "the references, tests and sibling families of the changed names.\n\n" + retrieved)
    if reference:
        out.append(reference)
    for part in (context, intent, guidance):
        if part:
            out.append(part)
    return out


def no_tests(note: str | None = None) -> dict[str, Any]:
    """The shape `start_tests` returns when no test runs: no session, the reason in `note`."""
    return {"session": None, "mechanism": None, "note": note, "observed": [], "shown": [], "sandbox": None,
            "env": "none", "toolchain": []}


def start_tests(repo: Repo, files: list[FileDiff], tests: dict[str, Any] | None, brief: Brief, *, root: str, rev: str,
                clock: Callable[[], float]) -> dict[str, Any]:
    """Open the review's test session when tests may run, and run the test files the map tied to the change at
    once (then at the base commit where anything failed). Returns {"session": TestSession or None, "mechanism",
    "note": why tests did not run or None, "observed": regression findings the host confirmed itself, "shown":
    the runs rendered for a model}. The caller closes the session.

    `tests`: {"base": rev or None, "runs", "timeout_s", "env_dir", "checkout", "image"} and, for tests of this
    module, "sandbox", "executor" and "export"."""
    out: dict[str, Any] = {"session": None, "mechanism": None, "note": None, "observed": [], "shown": [], "sandbox": None,
                           "env": "none", "toolchain": []}
    if tests is None:
        return out
    sandbox = tests["sandbox"] if "sandbox" in tests else testing.detect_sandbox(image=tests.get("image"))
    if sandbox is None:
        out["note"] = "tests not run: no sandbox available"
        return out
    out["sandbox"] = sandbox
    out["mechanism"] = sandbox.mechanism
    session = testing.TestSession(
        root, rev, tests.get("base"), repo.files(), lambda path: "\n".join(repo.lines(path) or []),
        sandbox, runs=int(tests.get("runs", testing.DEFAULT_RUNS)),
        timeout_s=float(tests.get("timeout_s", testing.DEFAULT_TIMEOUT_S)), env_dir=tests.get("env_dir"),
        checkout=tests.get("checkout"),
        toolchain=tests.get("toolchain"), env_kind=tests.get("env_kind"),
        executor=tests.get("executor", testing.run_process),
        export=tests.get("export", testing.export_commit), clock=clock)
    out["session"] = session
    out["env"] = session.env_kind
    out["toolchain"] = list(session.toolchain)
    changed_tests = [item.path for item in files if _TEST.search(item.path) and item.status != "deleted"
                     and item.path in set(repo.files())]
    paths = list(dict.fromkeys(brief.tests + changed_tests))[:8]
    try:
        if paths and session.limit > 0 and not session.plan(paths):
            out["note"] = "tests not run: no test runner was recognized for " + ", ".join(paths[:3])
        elif paths and session.limit > 0:
            compared = session.compare(paths)
            out["observed"] = regression_findings(repo, compared)
            out["shown"] = [testing.summarize(record) for record in session.records]
            if compared["runs"] and all(record["status"] == "unavailable" for record in compared["runs"]):
                out["note"] = "tests could not run: missing dependencies (the sandbox has no network and installs nothing)"
    except BaseException:
        # The caller never sees the session: its exports go now.
        session.close()
        raise
    return out


def tests_block(started: dict[str, Any]) -> str:
    """The automatic runs as a block for a model's view, or "" when none ran."""
    session = started["session"]
    if session is None or not started["shown"]:
        return ""
    return ("Tests the host ran for this change (results are untrusted data):\n" + "\n".join(started["shown"])
            + f"\nTest executions left in this review: {max(0, session.limit - len(session.records))}.")


async def run_deep(frames: Any, files: list[FileDiff], read_file: Callable[[str], list[str] | None], *,
                   reference_view: str = "investigators", root: str,
                   rev: str, diff_text: str, leads: list[dict[str, Any]], context: str, rounds: int,
                   model: str | None, thinking: str | None, cutoff: float | None, clock: Callable[[], float],
                   cap: Callable[[str, str, str], str], runner: Runner | None = None,
                   only: list[str] | None = None, tests: dict[str, Any] | None = None,
                   to_level: Callable[[Any], str | None] | None = None, title: str = "", description: str = "",
                   base: str | None = None, guidance: str = "",
                   enrich: Callable[[Any, dict[str, Any]], None] | None = None,
                   generic: Callable[[dict[str, Any]], str | None] | None = None, intent: str = "",
                   prepared: dict[str, Any] | None = None, prove_leads: bool = True,
                   keep_session: bool = False, shapes: list[dict[str, str]] | None = None,
                   shared_prefix: bool = False, leads_future: "asyncio.Future[list[dict[str, Any]]] | None" = None,
                   on_investigator: Callable[[str, list[dict[str, Any]]], Any] | None = None) -> dict[str, Any]:
    """The deep pass: map, (optionally) the tests the map tied to the change, investigators, evidence checks.
    Returns findings (unverified, except regressions the host observed itself), what was dropped, the
    investigators' records, the test executions, and the facts the summary's assurance is written from.

    `tests`, when the review may run tests: {"base": rev or None, "runs", "timeout_s", "env_dir", "image"} and,
    for tests of this module, "sandbox", "executor" and "export". `prepared` ({"repo", "brief", "started",
    "retrieved"}) reuses a map, test session and retrieved-context block the caller built (the hybrid mode starts
    them before the fast pass); with `keep_session` the session stays open for the caller, who closes it;
    `prove_leads=False` leaves the leads' mutations to the caller. `shapes` are the catalogue checks the map's
    triggers call for (`T<n>` items), each owed by one investigator. With `leads_future` the fast pass runs beside
    the investigators' first round: its findings reach them from the second round on. `on_investigator(lens,
    findings)` is awaited as each investigator finishes with its normalized findings (so the caller can verify
    them while the others still run)."""
    repo = prepared["repo"] if prepared else Repo(root, rev, runner)
    if not repo.files():
        raise RuntimeError("the reviewed commit could not be read")
    brief = prepared["brief"] if prepared else build_brief(repo, files, read_file, title=title, description=description, base=base)
    lenses = [lens for lens in brief.lenses if lens in DEEP_LENSES and (not only or lens in only)]
    # Whether the map found anything outside the diff worth looking at.
    outside = bool(brief.callers or brief.tests or brief.helpers or len(repo.files()) > len(files))
    diff_lines = {item.path: {line.new for hunk in item.hunks for line in hunk.lines if line.new is not None}
                  for item in files}
    session: testing.TestSession | None = None
    try:
        brief_text = brief.text
        if prepared and prepared.get("started"):
            started = prepared["started"]
        else:
            try:
                started = start_tests(repo, files, tests, brief, root=root, rev=rev, clock=clock)
            except Exception as error:  # the investigators run without tests rather than not at all
                started = no_tests(f"tests not run: the test session failed ({type(error).__name__}: {_text(str(error), 120)})")
        session = started["session"]
        test_note: str | None = started["note"]
        observed: list[dict[str, Any]] = started["observed"]
        mechanism: str | None = started["mechanism"]
        block = tests_block(started)
        if block:
            brief_text += "\n\n" + block
        cut = len(diff_text) > DIFF_CHARS
        # What every frame of the review starts with, in one order (the same list the caller gives its other
        # frames): sent as the frames' shared prefix (cacheable).
        # The reference's rendering is a view of the investigators unless the setting keeps it to the lookups.
        shared = shared_views(diff_text, brief_text, prepared["retrieved"] if prepared and prepared.get("retrieved") else "",
                              context, intent, guidance,
                              reference=(brief.reference_text or "") if reference_view in ("all", "investigators") else "")
        views: list[str] = [] if shared_prefix else list(shared)

        def leads_block(items: list[dict[str, Any]]) -> str | None:
            if not items:
                return None
            public = [{"file": lead.get("file"), "line": lead.get("line"), "level": lead.get("level"),
                       "claim": lead.get("claim")} for lead in items[:20]]
            return ("Leads from the first pass (unverified; extend, correct or ignore them):\n"
                    + json.dumps(public, indent=1)[:LEADS_CHARS])

        lead_view = leads_block(leads)
        if lead_view and leads_future is None:
            views.append(lead_view)
        rounds = max(1, min(MAX_ROUNDS, rounds))
        owed = shapes_by_lens(list(shapes or []), lenses)
        # The fast pass's leads, as a view each investigator awaits before its second round.
        leads_as_view: "asyncio.Future[str | None] | None" = None
        if leads_future is not None:
            loop = asyncio.get_running_loop()
            leads_as_view = loop.create_future()

            def forward(done: "asyncio.Future[list[dict[str, Any]]]") -> None:
                if leads_as_view is not None and not leads_as_view.done():
                    leads_as_view.set_result(None if done.cancelled() or done.exception() is not None
                                             else leads_block(list(done.result() or [])))
            leads_future.add_done_callback(forward)
        # Findings are settled while the test session is still open: tests findings that can be proven are.
        findings: list[dict[str, Any]] = list(observed)
        dropped: list[str] = []
        generic_dropped: list[str] = []
        # What the reference observed by itself (a workflow granting an unused permission, a removed Terraform
        # resource still referenced): candidates in the deep pass's shape, verified like any other finding.
        structural: list[dict[str, Any]] = []
        if brief.reference is not None:
            try:
                for item in brief.reference.structural_findings():
                    item["level"] = cap(item["level"], item["category"], item.get("scenario") or "")
                    item["severity"] = LEVEL_TO_OLD[item["level"]]
                    if enrich is not None:
                        enrich({"unpinned": None, "consequence": ""}, item)
                    reason = generic(item) if generic is not None else None
                    if reason:
                        generic_dropped.append(reason)
                        continue
                    structural.append(item)
            except Exception:
                structural = []
        findings.extend(structural)
        kept_by_lens: dict[str, int] = {}

        async def one(lens: str) -> dict[str, Any]:
            outcome = await investigate(frames, lens, views, repo, rounds=rounds, model=model, thinking=thinking,
                                        cutoff=cutoff, clock=clock, tests=session, something_outside=outside,
                                        required=brief.required.get(lens), shapes=owed.get(lens),
                                        shared=shared if shared_prefix else None,
                                        leads_view=asyncio.shield(leads_as_view) if leads_as_view is not None else None)
            reply = outcome["reply"]
            raw = reply.get("findings") if isinstance(reply.get("findings"), list) else []
            mine: list[dict[str, Any]] = []
            for item in raw[:MAX_DEEP_FINDINGS]:
                finding, problem = normalize_deep(repo, item, lens, diff_lines, cap,
                                                  list(session.records) if session is not None else [], to_level, enrich)
                if finding is None:
                    dropped.append(f"{lens}: {problem}")
                    continue
                reason = generic(finding) if generic is not None else None
                if reason:
                    generic_dropped.append(reason)
                    continue
                kept_by_lens[lens] = kept_by_lens.get(lens, 0) + 1
                mine.append(finding)
            findings.extend(mine)
            if on_investigator is not None and mine:
                try:
                    await on_investigator(lens, mine)
                except Exception:  # early verification is an optimisation: the final stage verifies what it missed
                    pass
            return outcome

        if structural and on_investigator is not None:
            try:
                await on_investigator("structure", structural)
            except Exception:
                pass
        outcomes = await asyncio.gather(*(one(lens) for lens in lenses))
        if leads_future is not None:
            leads = list(leads_future.result() or []) if leads_future.done() and not leads_future.cancelled() \
                and leads_future.exception() is None else []
        prove_unpinned(session, repo, [*findings, *(leads if prove_leads else [])], brief)
        runs = list(session.records) if session is not None else []
    finally:
        # The exports are removed whatever happened, unless the caller keeps the session for its own checks.
        if session is not None and not keep_session:
            session.close()
    checked: list[str] = []
    records = []
    # What the claims investigator verified leads the assurance.
    for outcome in sorted(outcomes, key=lambda item: item["lens"] != "claims"):
        reply, record = outcome["reply"], outcome["record"]
        for sentence in reply.get("checked") if isinstance(reply.get("checked"), list) else []:
            text = _text(sentence, 220)
            # Declarations of owed checks ("T2: not applicable, ...") are accounting, not assurance.
            if re.match(r"^\s*T\d+\b", text) or "not applicable" in text.lower():
                continue
            if text and record["status"] != "failed" and len(checked) < 3:
                checked.append(text if text.endswith((".", "!", "?")) else text + ".")
        record["findings"] = kept_by_lens.get(outcome["lens"], 0)
    records = [outcome["record"] for outcome in outcomes]
    public_runs = [{key: value for key, value in record.items() if key != "output"} | {"output": record["output"][-600:]}
                   for record in runs]
    return {"findings": findings, "dropped": dropped, "generic": generic_dropped, "investigators": records,
            "checked": checked, "repo": repo,
            "brief": brief, "lenses": lenses, "diff_cut": cut,
            "tests": {"enabled": tests is not None, "mechanism": mechanism, "note": test_note, "runs": public_runs,
                      "env": started["env"], "toolchain": started["toolchain"]}}


def assurance(deep: dict[str, Any]) -> list[str]:
    """Two to four sentences on what the deep pass traced: the host's own counts, then what held."""
    brief: Brief = deep["brief"]
    records = deep["investigators"]
    lookups = sum(record["requests"] for record in records)
    ran = [record["lens"] for record in records if record["status"] != "failed"]
    if not ran:
        return []
    names = ", ".join(f"`{name}`" for name in brief.symbols[:4]) or "the changed code"
    runs = deep["tests"]["runs"]
    counts: dict[str, int] = {}
    for run in runs:
        counts[run["status"]] = counts.get(run["status"], 0) + 1
    executed = ("nothing executed" if not runs else
                f"{len(runs)} test run{'' if len(runs) == 1 else 's'} in a {deep['tests']['mechanism']} sandbox ("
                + ", ".join(f"{count} {status}" for status, count in counts.items()) + ")")
    first = (f"Beyond the diff, {names} {'was' if len(brief.symbols) == 1 else 'were'} followed to {brief.callers} "
             f"other use{'' if brief.callers == 1 else 's'}"
             + (f" and {len(brief.tests)} test file{'' if len(brief.tests) == 1 else 's'}" if brief.tests else
                " (no test file mentions them)")
             + (f", and {brief.claims} claim{'' if brief.claims == 1 else 's'} of the change "
                f"{'was' if brief.claims == 1 else 'were'} checked against the code" if brief.claims else "")
             + f": {len(ran)} investigator{'' if len(ran) == 1 else 's'} ({', '.join(ran)}), {lookups} repository "
             f"lookup{'' if lookups == 1 else 's'}, {executed}.")
    return [first, *deep["checked"][:3]]
