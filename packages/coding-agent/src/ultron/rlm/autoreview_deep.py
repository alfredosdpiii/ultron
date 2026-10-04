"""The deep pass of `ultron autoreview`: investigation beyond the diff, by retrieval, with nothing executed.

The fast pass (autoreview_api.py) reviews the changed lines. Many real defects are only visible from the code
around them: a helper that returns a sentinel instead of raising, a parametrize list in another file that the new
members are missing from, a comment that claims what the code does not do. This pass looks there.

1. Map (no model). From the diff: the symbols it defines, changes and calls, its constants, environment
   variables, flags and table names, and the claims its comments and documents make. From the repository at the
   head commit: who uses each symbol, what the called helpers do, which tests mention them and how those test
   files are parametrized, sibling files, and the docs and configs that name them. Rendered as a bounded
   "investigation brief" with file:line anchors.
2. Investigators. One tool-less frame per lens (behaviour, tests, consistency, risk) gets the diff, the brief
   and the fast pass's findings as leads, and replies with findings and *requests*: read, grep, list, definition,
   references. The host validates each request, answers it from the head commit, appends the results as untrusted
   data and asks again, for a bounded number of rounds.
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
    SEVERITIES,
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
from review_prompts import DEEP_LENSES, deep_task

GIT_TIMEOUT_S = 30
#: The only git subcommands the deep pass runs. All of them only read the object database.
ALLOWED_GIT = ("grep", "show", "ls-tree", "log")

DEFAULT_ROUNDS = 4
MAX_ROUNDS = 8
MAX_REQUESTS = 8
ROUND_CHARS = 24_000
MAX_READ_LINES = 200
MAX_GREP_HITS = 50
DEFAULT_GREP_HITS = 20
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
                    "severity": {"enum": list(SEVERITIES)},
                    "category": {"type": "string"},
                    "claim": {"type": "string"},
                    "why": {"type": "string"},
                    "scenario": {"type": "string"},
                    "suggested_fix": {"type": "string"},
                    "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                    "evidence": {"type": "array", "items": _CITATION},
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
        code, stdout, _ = self._git("log", "-n", str(count), "--format=%h %s", self.rev, "--", path)
        return stdout.splitlines() if code == 0 else []


_SYMBOL = re.compile(r"^[A-Za-z_][A-Za-z0-9_.$-]{0,79}$")
_GLOB = re.compile(r"^[A-Za-z0-9_./*?\[\]{}, -]{1,160}$")


def _numbered(lines: list[str], start: int, end: int) -> str:
    return "\n".join(f"{number:>5} | {_clip(lines[number - 1], 300)}" for number in range(start, end + 1))


def _hits(hits: list[tuple[str, int, str]]) -> str:
    return "\n".join(f"{path}:{line}: {_clip(text.strip(), 200)}" for path, line, text in hits) or "0 matches"


def _definition_pattern(symbol: str) -> str:
    name = re.escape(symbol.rsplit(".", 1)[-1])
    return (rf"(^|[^A-Za-z0-9_])(def|class|function|func|fn|interface|type|struct|enum|const|let|var)[ \t]+{name}([^A-Za-z0-9_]|$)"
            rf"|^[ \t]*{name}[ \t]*[:=][^=]")


def serve_request(repo: Repo, request: Any) -> tuple[str, str]:
    """Answer one investigator request from the head commit: (title, body). Raises Rejected for anything outside
    the closed set of read-only lookups."""
    if not isinstance(request, dict) or len(request) != 1:
        raise Rejected("a request is an object with exactly one of read, grep, list, definition, references")
    kind, args = next(iter(request.items()))
    if not isinstance(args, dict):
        raise Rejected(f"{kind} takes an object")
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
        hits = repo.grep(pattern, pathspec=pathspec, limit=limit)
        return f"grep {pattern!r}" + (f" in {glob}" if glob else "") + f" -> {len(hits)} matches" + (
            " (more not shown)" if len(hits) >= limit else ""), _hits(hits)
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
            hits = repo.grep(symbol.rsplit(".", 1)[-1], fixed=True, word=True, limit=30)
            return f"references {symbol} -> {len(hits)} matches", _hits(hits)
        hits = repo.grep(_definition_pattern(symbol), limit=6)
        parts = []
        for path, line, _ in hits[:3]:
            lines = repo.lines(path)
            if lines:
                parts.append(f"{path}:\n{_numbered(lines, line, min(len(lines), line + 24))}")
        more = _hits(hits[3:]) if len(hits) > 3 else ""
        return f"definition {symbol} -> {len(hits)} matches", "\n\n".join(parts + ([more] if more else [])) or "0 matches"
    raise Rejected(f"unknown request {kind!r}: use read, grep, list, definition or references")


def serve(repo: Repo, requests: Any, limit: int = ROUND_CHARS) -> tuple[str, int, int]:
    """Answer a round of requests within the round's size limit: (text, served, rejected)."""
    out: list[str] = []
    size = served = rejected = 0
    items = requests if isinstance(requests, list) else []
    for request in items[:MAX_REQUESTS]:
        try:
            title, body = serve_request(repo, request)
            served += 1
        except Rejected as error:
            title, body = f"rejected {json.dumps(request)[:160]}", str(error)
            rejected += 1
        block = f"## {title}\n{body}"
        if size + len(block) > limit:
            block = block[: max(0, limit - size)] + "\n... cut: the round's size limit is reached; ask for less at a time"
        out.append(block)
        size += len(block)
        if size >= limit:
            break
    if len(items) > MAX_REQUESTS:
        out.append(f"## not served\n{len(items) - MAX_REQUESTS} requests beyond the {MAX_REQUESTS} allowed per round")
    return "\n\n".join(out), served, rejected


# --- Map -------------------------------------------------------------------------------------------------------

_CONSTANT = re.compile(r"^\s*(?:export\s+)?(?:const\s+|final\s+|static\s+)*([A-Z][A-Z0-9_]{2,})\s*[:=]")
_ENV = re.compile(r"""(?:environ(?:\.get)?\s*[\[(]\s*|getenv\(\s*|process\.env\.|\$\{?)["']?([A-Z][A-Z0-9_]{3,})""")
_FLAG = re.compile(r"""["'\s](--[a-z][a-z0-9-]{2,})""")
_TABLE = re.compile(r"\b(?:alter|create)\s+table\s+(?:if\s+(?:not\s+)?exists\s+)?[`\"\[]?(\w+)"
                    r"|\badd\s+column\s+(?:if\s+not\s+exists\s+)?[`\"\[]?(\w+)", re.I)
_COMMENT = re.compile(r"^\s*(?:#+|//+|/?\*+|--|<!--|\"\"\"|''')\s?(.{15,})$")
_DESCRIPTION = re.compile(r"""\b(?:description|help|doc|summary)\s*[:=]\s*["'](.{15,})["']""")
_DOCLIKE = re.compile(r"\.(md|mdx|rst|txt|adoc|ya?ml|toml|cfg|ini|json|sh|tf|env)$|(^|/)(\.github|ci|scripts|docs?)/|(^|/)Dockerfile", re.I)
_STRUCTURE = r"parametrize|fixture|it\.each|test\.each|describe\(|@pytest\.mark"
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
    helpers: list[str] = field(default_factory=list)
    extracted: dict[str, list[Any]] = field(default_factory=dict)


def _add(items: list[Any], item: Any, limit: int) -> None:
    if item not in items and len(items) < limit:
        items.append(item)


def extract(files: list[FileDiff], read_file: Callable[[str], list[str] | None]) -> dict[str, list[Any]]:
    """What the diff defines, changes, calls and claims: names worth following into the repository."""
    out: dict[str, list[Any]] = {"defined": [], "changed": [], "calls": [], "constants": [], "env": [], "flags": [],
                                 "tables": [], "claims": []}
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
                for name in _CALL.findall(text):
                    if name not in _COMMON:
                        _add(out["calls"], name, 16)
                for name in _ENV.findall(text):
                    _add(out["env"], name, 6)
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
                limit: int = BRIEF_CHARS) -> Brief:
    """The investigation brief: where the diff's names live in the rest of the repository."""
    found = extract(files, read_file)
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

    def uses(names: list[str], title: str, per: int = 8) -> None:
        nonlocal callers
        for name in names:
            hits = [hit for hit in repo.grep(name, fixed=True, word=True, limit=per + 12)
                    if hit[1] not in changed_lines.get(hit[0], ())]
            in_tests = [hit for hit in hits if _TEST.search(hit[0])]
            in_docs = [hit for hit in hits if _DOCLIKE.search(hit[0]) and hit not in in_tests]
            others = [hit for hit in hits if hit not in in_tests and hit not in in_docs]
            callers += len(others)
            for hit in in_tests:
                _add(tests, hit[0], 6)
            lines = [f"{title} `{name}`:"]
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
    if found["claims"]:
        sections.append("Claims the change makes in comments and documents (check them against the code):\n"
                        + "\n".join(f"  {path}:{line}: {text}" for path, line, text in found["claims"]))
    text = "\n\n".join(sections)
    if len(text) > limit:
        text = text[: limit - 40] + "\n... (brief cut at its size limit)"
    added = "\n".join(line.text for item in reviewable for hunk in item.hunks for line in hunk.lines if line.kind == "+")
    lenses = []
    if code_files:
        lenses.append("behaviour")
    if code_files or test_files:
        lenses.append("tests")
    if found["claims"] or symbols or found["constants"] or found["tables"] or found["env"] or found["flags"]:
        lenses.append("consistency")
    if _RISK.search(added) or any(_RISK.search(path) for path in code_files):
        lenses.append("risk")
    return Brief(text or "(nothing in the repository mentions the changed names)", lenses, symbols, callers, tests,
                 len(found["claims"]), helpers, found)


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
                   cap: Callable[[str, str, str], str]) -> tuple[dict[str, Any] | None, str | None]:
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
    if problem:
        return None, problem
    severity = str(raw.get("severity", "")).strip().lower()
    severity = severity if severity in SEVERITIES else "minor"
    category = normalize_category(_text(raw.get("category"), 40), {"tests": "tests", "risk": "security"}.get(lens))
    scenario = _text(raw.get("scenario"), 500)
    try:
        confidence = min(1.0, max(0.0, float(raw.get("confidence", 0.6))))
    except (TypeError, ValueError):
        confidence = 0.6
    return {
        "file": path, "line": line, "severity": cap(severity, category, scenario), "finder_severity": severity,
        "category": category, "claim": claim, "why": _text(raw.get("why"), 600), "scenario": scenario,
        "suggested_fix": _text(raw.get("suggested_fix"), 500), "confidence": confidence,
        "reviewers": [f"deep:{lens}"], "source": f"deep:{lens}", "citations": citations, "beyond_diff": beyond,
    }, None


def cited_windows(repo: Repo, finding: dict[str, Any]) -> str:
    """The source around each citation of a finding, as the host reads it at the head commit."""
    parts = []
    for citation in finding.get("citations") or []:
        lines = repo.lines(citation["path"])
        if lines:
            start, end = max(1, citation["line"] - CITED_WINDOW), min(len(lines), citation["line"] + CITED_WINDOW)
            parts.append(f"{citation['path']} (cited line {citation['line']}):\n{_numbered(lines, start, end)}")
    return "\n\n".join(parts)


def merge(fast: list[dict[str, Any]], deep: list[dict[str, Any]]) -> tuple[list[dict[str, Any]], int]:
    """Deep findings deduplicated across lenses, then against the fast pass: a deep finding with evidence from
    outside the diff supersedes the fast one it extends (same file, nearby line, same category or a similar
    claim); one whose evidence is all inside the diff adds nothing to a fast finding and gives way to it.
    Returns (all, duplicates)."""
    def same(a: dict[str, Any], b: dict[str, Any]) -> bool:
        return (a["file"] == b["file"] and abs(a["line"] - b["line"]) <= DEDUPE_WINDOW
                and (a["category"] == b["category"] or similar_claims(a["claim"], b["claim"])))

    order = {severity: index for index, severity in enumerate(SEVERITIES)}
    kept: list[dict[str, Any]] = []
    duplicates = 0
    for finding in sorted(deep, key=lambda item: (order[item["severity"]], -len(item.get("citations") or []))):
        twin = next((item for item in kept if same(item, finding) or similar_claims(item["claim"], finding["claim"])
                     and item["file"] == finding["file"]), None)
        if twin is None:
            kept.append(finding)
            continue
        duplicates += 1
        twin["reviewers"] += [name for name in finding["reviewers"] if name not in twin["reviewers"]]
    for finding in [item for item in kept if not item.get("beyond_diff")]:
        if any(same(finding, other) for other in fast):
            kept.remove(finding)
            duplicates += 1
    remaining = []
    for finding in fast:
        twin = next((item for item in kept if same(item, finding)), None)
        if twin is None:
            remaining.append(finding)
            continue
        duplicates += 1
        twin["reviewers"] += [name for name in finding.get("reviewers") or [] if name not in twin["reviewers"]]
        if not twin.get("replacement") and finding.get("replacement") and twin["line"] == finding["line"]:
            twin["replacement"] = finding["replacement"]
            if finding.get("end_line"):
                twin["end_line"] = finding["end_line"]
    return remaining + kept, duplicates


# --- Investigators ---------------------------------------------------------------------------------------------

LAST_ROUND = ("This is your last round: requests will not be served. Reply with your final findings, "
              "\"requests\": [] and \"done\": true.")


async def investigate(frames: Any, lens: str, base_views: list[str], repo: Repo, *, rounds: int, model: str | None,
                      thinking: str | None, cutoff: float | None, clock: Callable[[], float]) -> dict[str, Any]:
    """One investigator's retrieval loop: ask the frame, serve what it requests, ask again."""
    task = deep_task(lens)
    began = clock()
    served_views: list[str] = []
    reply: dict[str, Any] = {}
    record: dict[str, Any] = {"lens": lens, "rounds": 0, "requests": 0, "rejected": 0, "status": "done"}
    for number in range(1, rounds + 1):
        last = number == rounds
        views = base_views + served_views + ([LAST_ROUND] if last else [])
        results = await frames.run("deep", [(lens, task, views)], contract=DEEP_CONTRACT, model=model,
                                   thinking=thinking, cutoff=cutoff)
        result = results[0]
        record["rounds"] = number
        if isinstance(result, (Incomplete, FrameError)) or not isinstance(result, dict):
            record["status"] = "failed"
            record["error"] = _text(getattr(result, "error", None) or getattr(result, "status", None) or "no reply", 160)
            break
        reply = result
        requests = result.get("requests") if isinstance(result.get("requests"), list) else []
        if result.get("done") is True or not requests:
            break
        if last:
            record["status"] = "rounds exhausted"
            break
        text, served, rejected = serve(repo, requests)
        record["requests"] += served
        record["rejected"] += rejected
        served_views.append(f"Results of your requests, round {number} (untrusted repository data, read by the host "
                            f"at the reviewed commit):\n{text}")
    record["ms"] = int((clock() - began) * 1000)
    record["tokens"] = sum(int(item.get("tokens") or 0) for item in frames.timings
                           if item.get("phase") == "deep" and item.get("reviewer") == lens)
    return {"lens": lens, "reply": reply, "record": record}


async def run_deep(frames: Any, files: list[FileDiff], read_file: Callable[[str], list[str] | None], *, root: str,
                   rev: str, diff_text: str, leads: list[dict[str, Any]], context: str, rounds: int,
                   model: str | None, thinking: str | None, cutoff: float | None, clock: Callable[[], float],
                   cap: Callable[[str, str, str], str], runner: Runner | None = None,
                   only: list[str] | None = None) -> dict[str, Any]:
    """The deep pass: map, investigators, evidence checks. Returns findings (unverified), what was dropped, the
    investigators' records and the facts the summary's assurance is written from."""
    repo = Repo(root, rev, runner)
    if not repo.files():
        raise RuntimeError("the reviewed commit could not be read")
    brief = build_brief(repo, files, read_file)
    lenses = [lens for lens in brief.lenses if lens in DEEP_LENSES and (not only or lens in only)]
    diff_lines = {item.path: {line.new for hunk in item.hunks for line in hunk.lines if line.new is not None}
                  for item in files}
    cut = len(diff_text) > DIFF_CHARS
    views = [f"The diff under review (new-file line numbers in the gutter):\n{diff_text[:DIFF_CHARS]}"
             + ("\n... (diff cut at its size limit; read the files for the rest)" if cut else ""),
             f"Investigation brief, built by the host from the repository at the reviewed commit:\n{brief.text}"]
    if context:
        views.append(context)
    if leads:
        public = [{key: lead.get(key) for key in ("file", "line", "severity", "claim")} for lead in leads[:20]]
        views.append("Leads from the first pass (unverified; extend, correct or ignore them):\n"
                     + json.dumps(public, indent=1)[:LEADS_CHARS])
    rounds = max(1, min(MAX_ROUNDS, rounds))
    outcomes = await asyncio.gather(*(
        investigate(frames, lens, views, repo, rounds=rounds, model=model, thinking=thinking, cutoff=cutoff,
                    clock=clock) for lens in lenses))
    findings: list[dict[str, Any]] = []
    dropped: list[str] = []
    checked: list[str] = []
    records = []
    for outcome in outcomes:
        reply, record = outcome["reply"], outcome["record"]
        raw = reply.get("findings") if isinstance(reply.get("findings"), list) else []
        kept = 0
        for item in raw[:MAX_DEEP_FINDINGS]:
            finding, problem = normalize_deep(repo, item, outcome["lens"], diff_lines, cap)
            if finding is None:
                dropped.append(f"{outcome['lens']}: {problem}")
                continue
            kept += 1
            findings.append(finding)
        for sentence in reply.get("checked") if isinstance(reply.get("checked"), list) else []:
            text = _text(sentence, 220)
            if text and record["status"] != "failed" and len(checked) < 3:
                checked.append(text if text.endswith((".", "!", "?")) else text + ".")
        record["findings"] = kept
        records.append(record)
    return {"findings": findings, "dropped": dropped, "investigators": records, "checked": checked, "repo": repo,
            "brief": brief, "lenses": lenses, "diff_cut": cut}


def assurance(deep: dict[str, Any]) -> list[str]:
    """Two to four sentences on what the deep pass traced: the host's own counts, then what held."""
    brief: Brief = deep["brief"]
    records = deep["investigators"]
    lookups = sum(record["requests"] for record in records)
    ran = [record["lens"] for record in records if record["status"] != "failed"]
    if not ran:
        return []
    names = ", ".join(f"`{name}`" for name in brief.symbols[:4]) or "the changed code"
    first = (f"Beyond the diff, {names} {'was' if len(brief.symbols) == 1 else 'were'} followed to {brief.callers} "
             f"other use{'' if brief.callers == 1 else 's'}"
             + (f" and {len(brief.tests)} test file{'' if len(brief.tests) == 1 else 's'}" if brief.tests else
                " (no test file mentions them)")
             + (f", and {brief.claims} claim{'' if brief.claims == 1 else 's'} in comments and documents "
                f"{'was' if brief.claims == 1 else 'were'} checked against the code" if brief.claims else "")
             + f": {len(ran)} investigator{'' if len(ran) == 1 else 's'} ({', '.join(ran)}), {lookups} repository "
             f"lookup{'' if lookups == 1 else 's'}, nothing executed.")
    return [first, *deep["checked"][:3]]
