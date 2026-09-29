"""`/review`: a finder-then-verifier code review over bounded inference frames.

The `/review` command asks the model to run one cell:

    import review_api
    review = await review_api.run(rlm, "<the command's arguments>")
    print(review.report)

1. Scope. The diff comes from git (working tree plus the branch since its merge base with the default branch, or
   since a given ref) or from `gh pr diff`. Files are parsed into hunks; lockfiles, generated, binary and deleted
   files are skipped and listed as not checked.
2. Chunks. Each file's hunks are rendered with new-file line numbers and unchanged code from the repository around
   them, packed into chunks of bounded size (a large hunk is split).
3. Find. One `rlm.map` over (reviewer x chunk): five specialists (correctness, security, architecture, tests,
   AI/LLM integration), each a frame with its own checklist and a findings contract. Frames, not sub-agents: a
   finder only needs the chunk it is given, and a frame costs one request where a sub-agent re-sends a system
   prompt and transcript every turn.
4. Dedupe. Findings on the same file, nearby lines and the same category (or the same claim) merge into one.
5. Verify. One `rlm.map` of verifier frames, one per finding, each seeing the cited source window, the hunk, and the
   definitions and uses of the names involved (found with `git grep`). Rejected findings are dropped; a confirmation
   whose evidence does not quote the source is downgraded to uncertain. With `--deep`, uncertain findings go to a
   sub-agent that can explore the repository: that is the one place a reviewer needs more than a fixed view.
6. Report. Grouped by severity, with the real source line quoted by code, counts, cost and what was not checked.

Everything runs under one token cap (`--budget`); frames past it come back `Incomplete` and are reported as
not checked. Nothing is posted unless `post(review, confirm=True)` is called after the user agreed.
"""
from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from infer_api import Budget, FrameError, Incomplete
from review_prompts import ALIASES, DEEP_VERIFY_BRIEF, REVIEWERS, VERIFIER_TASK, Reviewer, finder_task

SEVERITIES = ("blocker", "major", "minor", "nit")
SEVERITY_TITLES = {"blocker": "Blockers", "major": "Major", "minor": "Minor", "nit": "Nits"}
DEFAULT_BUDGET_TOKENS = 300_000
MIN_BUDGET_TOKENS = 10_000
FIND_SHARE = 0.6
CHUNK_CHARS = 14_000
CONTEXT_LINES = 12
VERIFY_WINDOW = 15
DEDUPE_WINDOW = 3
MAX_FINDINGS_PER_FRAME = 8
MAX_LINE_CHARS = 400
FRAME_TIMEOUT_MS = 240_000
GIT_TIMEOUT_S = 30
GH_TIMEOUT_S = 60
MAX_UNTRACKED_FILES = 40
MAX_UNTRACKED_BYTES = 200_000
RELATED_CHARS = 3_000
DEEP_DEFAULT = 3
DEEP_MAX = 10
DEEP_TIMEOUT_MS = 300_000
REPORT_FINDINGS = 30
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"

USAGE = """Usage: /review [base-ref | PR-number | path ...] [options]

  (no target)       uncommitted changes plus the current branch since its merge base with the default branch
  <base-ref>        changes since the merge base with a branch, tag or commit
  <PR-number>       a GitHub pull request, through `gh pr diff` (also #123 or a PR URL)
  <path ...>        only these files or directories (combine with a ref or PR; `--` ends options)

  --only a,b        reviewers to run: bugs, security, arch, tests, ai (default: all)
  --budget N        token cap for all frames, e.g. 200k or 1m (default 300k)
  --model p/m       model for the frames (default: review.model, else rlm.frameModel, else the session's model)
  --deep[=N]        re-check up to N uncertain findings with a sub-agent that explores the repo (default 3)
  --plan            show the scope, chunks and frame plan without calling a model
  --post            prepare the report for posting to the PR; posting still needs your explicit yes"""

FINDINGS_CONTRACT: dict[str, Any] = {
    "type": "array",
    "maxItems": MAX_FINDINGS_PER_FRAME,
    "items": {
        "type": "object",
        "properties": {
            "file": {"type": "string"},
            "line": {"type": "integer"},
            "severity": {"enum": list(SEVERITIES)},
            "category": {"type": "string"},
            "claim": {"type": "string"},
            "why": {"type": "string"},
            "suggested_fix": {"type": "string"},
            "confidence": {"type": "number", "minimum": 0, "maximum": 1},
        },
        "required": ["file", "line", "severity", "category", "claim", "why", "suggested_fix", "confidence"],
    },
}

VERDICT_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {
        "verdict": {"enum": ["confirmed", "rejected", "uncertain"]},
        "evidence": {"type": "string"},
        "corrected_line": {"type": ["integer", "null"]},
    },
    "required": ["verdict", "evidence"],
}


class ReviewError(Exception):
    """A request `/review` cannot run: bad arguments, not a repository, no `gh`, an unknown ref."""


# --- Arguments -------------------------------------------------------------------------------------------------


@dataclass
class Options:
    targets: list[str] = field(default_factory=list)
    explicit_paths: list[str] = field(default_factory=list)
    only: list[str] | None = None
    budget_tokens: int = DEFAULT_BUDGET_TOKENS
    model: str | None = None
    # `review.model` (/settings -> Models) when neither --model nor ULTRON_REVIEW_MODEL names one; frames only.
    frame_model: str | None = None
    post: bool = False
    deep: int = 0
    plan: bool = False
    help: bool = False

    @property
    def reviewers(self) -> list[Reviewer]:
        keys = self.only or list(REVIEWERS)
        return [REVIEWERS[key] for key in keys]


def _tokens(value: str) -> int:
    match = re.fullmatch(r"(\d+(?:\.\d+)?)([km]?)", value.strip().lower().replace(",", "").replace("_", ""))
    if not match:
        raise ReviewError(f"--budget takes a token count such as 200000, 200k or 1.5m, not {value!r}")
    amount = float(match.group(1)) * {"": 1, "k": 1_000, "m": 1_000_000}[match.group(2)]
    if amount < MIN_BUDGET_TOKENS:
        raise ReviewError(f"--budget must be at least {MIN_BUDGET_TOKENS:,} tokens")
    return int(amount)


def _reviewer_keys(value: str) -> list[str]:
    keys: list[str] = []
    for raw in value.split(","):
        name = raw.strip().lower()
        if not name:
            continue
        key = ALIASES.get(name, name)
        if key not in REVIEWERS:
            raise ReviewError(f"unknown reviewer {raw.strip()!r}; choose from {', '.join(REVIEWERS)}")
        if key not in keys:
            keys.append(key)
    if not keys:
        raise ReviewError("--only needs at least one reviewer")
    return keys


def parse_args(text: str | list[str] | None, env: dict[str, str] | None = None) -> Options:
    """Parse `/review`'s argument string (shell-style quoting). `env` supplies defaults for the flags:
    ULTRON_REVIEW_BUDGET, ULTRON_REVIEW_MODEL and ULTRON_REVIEW_ONLY."""
    if isinstance(text, list):
        tokens = list(text)
    else:
        try:
            tokens = shlex.split(text or "")
        except ValueError as error:
            raise ReviewError(f"cannot parse the arguments: {error}") from None
    options = Options()
    env = env or {}
    if env.get("ULTRON_REVIEW_BUDGET", "").strip():
        options.budget_tokens = _tokens(env["ULTRON_REVIEW_BUDGET"])
    if env.get("ULTRON_REVIEW_ONLY", "").strip():
        options.only = _reviewer_keys(env["ULTRON_REVIEW_ONLY"])
    if env.get("ULTRON_REVIEW_MODEL", "").strip():
        tokens = ["--model", env["ULTRON_REVIEW_MODEL"].strip(), *tokens]
    index = 0

    def value(flag: str, inline: str | None) -> str:
        nonlocal index
        if inline is not None:
            return inline
        index += 1
        if index >= len(tokens):
            raise ReviewError(f"{flag} needs a value")
        return tokens[index]

    while index < len(tokens):
        token = tokens[index]
        flag, _, inline_value = token.partition("=")
        inline = inline_value if "=" in token else None
        if token == "--":
            options.explicit_paths.extend(tokens[index + 1:])
            break
        if flag in ("-h", "--help"):
            options.help = True
        elif flag == "--only":
            options.only = _reviewer_keys(value(flag, inline))
        elif flag == "--budget":
            options.budget_tokens = _tokens(value(flag, inline))
        elif flag == "--model":
            model = value(flag, inline).strip()
            if not re.fullmatch(r"[^/\s]+/[^/\s]+(?:/[^/\s]+)*", model):
                raise ReviewError(f"--model takes provider/model, not {model!r}")
            options.model = model
        elif flag == "--post":
            options.post = True
        elif flag in ("--plan", "--dry-run"):
            options.plan = True
        elif flag == "--deep":
            if inline is None:
                options.deep = DEEP_DEFAULT
            elif inline.isdigit() and 1 <= int(inline) <= DEEP_MAX:
                options.deep = int(inline)
            else:
                raise ReviewError(f"--deep=N takes a number from 1 to {DEEP_MAX}")
        elif token.startswith("-") and token != "-":
            raise ReviewError(f"unknown option {token}\n\n{USAGE}")
        else:
            options.targets.append(token)
        index += 1
    return options


@dataclass
class Target:
    pr: int | None = None
    base: str | None = None
    paths: list[str] = field(default_factory=list)


_PR_URL = re.compile(r"^https?://[^/]+/[^/]+/[^/]+/pull/(\d+)(?:[/?#].*)?$")


def classify_targets(options: Options, *, exists: Callable[[str], bool], is_ref: Callable[[str], bool]) -> Target:
    """Sort positional arguments into at most one PR or base ref, and paths."""
    target = Target(paths=list(options.explicit_paths))
    for token in options.targets:
        number = re.fullmatch(r"#?(\d+)", token)
        url = _PR_URL.match(token)
        if (number and not exists(token)) or url:
            pr = int((number or url).group(1))  # type: ignore[union-attr]
            if target.pr is not None and target.pr != pr:
                raise ReviewError("give at most one pull request")
            target.pr = pr
        elif exists(token):
            target.paths.append(token)
        elif is_ref(token):
            if target.base is not None:
                raise ReviewError(f"give at most one base ref (got {target.base!r} and {token!r})")
            target.base = token
        else:
            raise ReviewError(f"{token!r} is not a path in this repository, a git ref, or a PR number")
    if target.pr is not None and target.base is not None:
        raise ReviewError("give a PR number or a base ref, not both")
    return target


# --- Processes -------------------------------------------------------------------------------------------------

Runner = Callable[[list[str], str, float], "tuple[int, str, str]"]


def _run_process(argv: list[str], cwd: str, timeout: float) -> tuple[int, str, str]:
    env = dict(os.environ, GIT_PAGER="cat", PAGER="cat", GIT_OPTIONAL_LOCKS="0", GH_PROMPT_DISABLED="1",
               GIT_TERMINAL_PROMPT="0", NO_COLOR="1")
    try:
        done = subprocess.run(argv, cwd=cwd, capture_output=True, timeout=timeout, env=env, stdin=subprocess.DEVNULL)
    except FileNotFoundError:
        return 127, "", f"{argv[0]}: not found"
    except subprocess.TimeoutExpired:
        return 124, "", f"{' '.join(argv[:3])}: timed out after {timeout:.0f}s"
    return done.returncode, done.stdout.decode("utf-8", "replace"), done.stderr.decode("utf-8", "replace")


class Git:
    def __init__(self, cwd: str, runner: Runner | None = None) -> None:
        self.cwd = cwd
        self._runner = runner or _run_process

    def call(self, *args: str, timeout: float = GIT_TIMEOUT_S) -> tuple[int, str, str]:
        return self._runner(["git", *args], self.cwd, timeout)

    def out(self, *args: str) -> str:
        code, stdout, stderr = self.call(*args)
        if code != 0:
            raise ReviewError(f"git {' '.join(args[:2])} failed: {(stderr or stdout).strip()[:300]}")
        return stdout

    def ok(self, *args: str) -> bool:
        return self.call(*args)[0] == 0

    def value(self, *args: str) -> str | None:
        code, stdout, _ = self.call(*args)
        return stdout.strip() or None if code == 0 else None


# --- Diff parsing ----------------------------------------------------------------------------------------------


@dataclass
class DiffLine:
    kind: str  # "+", "-" or " "
    old: int | None
    new: int | None
    text: str


@dataclass
class Hunk:
    old_start: int
    old_len: int
    new_start: int
    new_len: int
    header: str
    lines: list[DiffLine] = field(default_factory=list)

    @property
    def new_first(self) -> int:
        numbers = [line.new for line in self.lines if line.new is not None]
        return numbers[0] if numbers else max(1, self.new_start)

    @property
    def new_last(self) -> int:
        numbers = [line.new for line in self.lines if line.new is not None]
        return numbers[-1] if numbers else max(1, self.new_start)


@dataclass
class FileDiff:
    path: str
    old_path: str | None = None
    status: str = "modified"  # added, deleted, renamed, modified
    binary: bool = False
    hunks: list[Hunk] = field(default_factory=list)

    @property
    def added(self) -> int:
        return sum(1 for hunk in self.hunks for line in hunk.lines if line.kind == "+")

    @property
    def removed(self) -> int:
        return sum(1 for hunk in self.hunks for line in hunk.lines if line.kind == "-")


_HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)$")
_DIFF_GIT = re.compile(r'^diff --git "?[a-z]/(.*?)"? "?[a-z]/(.*?)"?$')
_PREFIX = re.compile(r"^[a-z]/")


def _unquote(path: str) -> str:
    path = path.strip()
    if path.startswith('"') and path.endswith('"'):
        try:
            return json.loads(path)
        except ValueError:
            return path[1:-1]
    return path


def parse_diff(text: str) -> list[FileDiff]:
    """Parse unified git diff text (from `git diff` or `gh pr diff`) into files and hunks."""
    files: list[FileDiff] = []
    current: FileDiff | None = None
    hunk: Hunk | None = None
    old_left = new_left = 0
    old_no = new_no = 0
    for raw in text.splitlines():
        if hunk is not None and (old_left > 0 or new_left > 0):
            marker, body = (raw[:1], raw[1:]) if raw else (" ", "")
            if marker == "\\":
                continue
            if marker == "+":
                hunk.lines.append(DiffLine("+", None, new_no, body))
                new_no += 1
                new_left -= 1
                continue
            if marker == "-":
                hunk.lines.append(DiffLine("-", old_no, None, body))
                old_no += 1
                old_left -= 1
                continue
            if marker == " ":
                hunk.lines.append(DiffLine(" ", old_no, new_no, body))
                old_no += 1
                new_no += 1
                old_left -= 1
                new_left -= 1
                continue
            hunk = None  # malformed: fall through to header parsing
        if raw.startswith("diff --git "):
            match = _DIFF_GIT.match(raw)
            old, new = (match.group(1), match.group(2)) if match else (None, raw.split(" b/")[-1])
            current = FileDiff(path=new, old_path=old)
            files.append(current)
            hunk = None
            continue
        if current is None:
            continue
        if raw.startswith("new file mode"):
            current.status = "added"
            current.old_path = None
        elif raw.startswith("deleted file mode"):
            current.status = "deleted"
        elif raw.startswith("rename from "):
            current.old_path = _unquote(raw[len("rename from "):])
            current.status = "renamed"
        elif raw.startswith("rename to "):
            current.path = _unquote(raw[len("rename to "):])
            current.status = "renamed"
        elif raw.startswith("Binary files ") or raw.startswith("GIT binary patch"):
            current.binary = True
        elif raw.startswith("--- "):
            source = _unquote(raw[4:].split("\t")[0])
            if source != "/dev/null" and _PREFIX.match(source) and current.status != "added":
                current.old_path = source[2:]
        elif raw.startswith("+++ "):
            dest = _unquote(raw[4:].split("\t")[0])
            if dest == "/dev/null":
                current.status = "deleted"
            elif _PREFIX.match(dest):
                current.path = dest[2:]
        else:
            match = _HUNK.match(raw)
            if match:
                old_start, old_len = int(match.group(1)), int(match.group(2) or "1")
                new_start, new_len = int(match.group(3)), int(match.group(4) or "1")
                hunk = Hunk(old_start, old_len, new_start, new_len, match.group(5).strip())
                current.hunks.append(hunk)
                old_left, new_left = old_len, new_len
                old_no, new_no = old_start, new_start
    for item in files:
        if item.status == "modified" and item.old_path and item.old_path != item.path:
            item.status = "renamed"
    return files


def added_file(path: str, text: str) -> FileDiff:
    """A synthetic diff for an untracked file: every line added."""
    lines = text.splitlines()
    hunk = Hunk(0, 0, 1, len(lines), "", [DiffLine("+", None, number, body) for number, body in enumerate(lines, 1)])
    return FileDiff(path=path, status="added", hunks=[hunk] if lines else [])


_GENERATED = re.compile(
    r"(^|/)(package-lock\.json|npm-shrinkwrap\.json|yarn\.lock|pnpm-lock\.yaml|Cargo\.lock|poetry\.lock|Pipfile\.lock"
    r"|composer\.lock|Gemfile\.lock|go\.sum|uv\.lock|bun\.lockb?)$|\.min\.(js|css)$|\.map$|\.snap$|\.generated\."
    r"|(^|/)(node_modules|vendor|dist|build)/"
)
_BINARY_EXT = re.compile(r"\.(png|jpe?g|gif|webp|ico|pdf|zip|gz|tgz|jar|wasm|woff2?|ttf|otf|mp[34]|mov|so|dylib|dll|exe)$", re.I)
_DOC = re.compile(r"\.(md|mdx|rst|txt|adoc)$", re.I)
_TEST = re.compile(r"(^|/)(tests?|__tests__|spec|specs)/|[._-](test|spec)\.[^/]+$|(^|/)test_[^/]+\.py$", re.I)


def skip_reason(item: FileDiff) -> str | None:
    if item.binary or _BINARY_EXT.search(item.path):
        return "binary"
    if item.status == "deleted":
        return "deleted"
    if _GENERATED.search(item.path):
        return "generated, lockfile or vendored"
    if not any(line.kind == "+" for hunk in item.hunks for line in hunk.lines):
        return "no added lines (rename, mode change or removal only)"
    return None


def file_kind(path: str) -> str:
    if _DOC.search(path):
        return "doc"
    if _TEST.search(path):
        return "test"
    return "code"


# --- Chunks ----------------------------------------------------------------------------------------------------


@dataclass
class Chunk:
    id: int
    path: str
    kind: str
    status: str
    hunks: list[Hunk]
    text: str
    part: int = 1
    parts: int = 1


def _clip(text: str, limit: int = MAX_LINE_CHARS) -> str:
    return text if len(text) <= limit else text[: limit - 1] + "…"


def render_hunk(hunk: Hunk) -> list[str]:
    out = []
    for line in hunk.lines:
        number = "" if line.new is None else str(line.new)
        mark = line.kind if line.kind != " " else " "
        out.append(f"{number:>5} {mark} {_clip(line.text)}")
    return out


def _context(source: list[str] | None, start: int, end: int) -> list[str]:
    """Unchanged source lines [start, end] (1-based, inclusive), gutter-numbered."""
    if not source or end < start:
        return []
    start, end = max(1, start), min(len(source), end)
    return [f"{number:>5}   {_clip(source[number - 1])}" for number in range(start, end + 1)]


def _split_hunk(hunk: Hunk, max_lines: int) -> list[Hunk]:
    pieces = []
    for offset in range(0, len(hunk.lines), max_lines):
        lines = hunk.lines[offset: offset + max_lines]
        news = [line.new for line in lines if line.new is not None]
        pieces.append(Hunk(hunk.old_start, 0, news[0] if news else hunk.new_start, len(news), hunk.header, lines))
    return pieces


def build_chunks(files: list[FileDiff], read_file: Callable[[str], list[str] | None], *,
                 max_chars: int = CHUNK_CHARS, context: int = CONTEXT_LINES) -> tuple[list[Chunk], list[tuple[str, str]]]:
    """Render reviewable files as gutter-numbered chunks with surrounding source; return (chunks, skipped)."""
    chunks: list[Chunk] = []
    skipped: list[tuple[str, str]] = []
    changed = [item.path for item in files]
    for item in files:
        reason = skip_reason(item)
        if reason:
            skipped.append((item.path, reason))
            continue
        source = read_file(item.path)
        others = [path for path in changed if path != item.path]
        listing = ", ".join(others[:30]) + (f" (+{len(others) - 30} more)" if len(others) > 30 else "")
        max_lines = max(20, max_chars // 120)
        hunks = [piece for hunk in item.hunks
                 for piece in ([hunk] if len(hunk.lines) <= max_lines else _split_hunk(hunk, max_lines))]
        blocks: list[tuple[Hunk, str]] = []
        for position, hunk in enumerate(hunks):
            before_floor = hunks[position - 1].new_last + 1 if position > 0 else 1
            after_ceiling = hunks[position + 1].new_first - 1 if position + 1 < len(hunks) else hunk.new_last + context
            lines = [f"@@ {hunk.header}".rstrip() if hunk.header else "@@"]
            lines += _context(source, max(before_floor, hunk.new_first - context), hunk.new_first - 1)
            lines += render_hunk(hunk)
            lines += _context(source, hunk.new_last + 1, min(after_ceiling, hunk.new_last + context))
            blocks.append((hunk, "\n".join(lines)))
        groups: list[list[tuple[Hunk, str]]] = [[]]
        size = 0
        for block in blocks:
            if groups[-1] and size + len(block[1]) > max_chars:
                groups.append([])
                size = 0
            groups[-1].append(block)
            size += len(block[1])
        for part, group in enumerate(groups, 1):
            label = f"{item.status}" + (f", part {part} of {len(groups)}" if len(groups) > 1 else "")
            header = [f"File: {item.path} ({label})"]
            if item.old_path and item.status == "renamed":
                header.append(f"Renamed from: {item.old_path}")
            if listing:
                header.append(f"Other files changed in this review: {listing}")
            if source is None and item.status != "added":
                header.append("Surrounding source was not available; only the diff is shown.")
            text = "\n".join(header) + "\n\n" + "\n\n".join(block for _, block in group)
            chunks.append(Chunk(len(chunks) + 1, item.path, file_kind(item.path), item.status,
                                [hunk for hunk, _ in group], text, part, len(groups)))
    return chunks, skipped


_LLM_HINT = re.compile(
    r"openai|anthropic|\bllm|language model|chat[._]?completion|completions?\.create|messages\.create|\bprompt"
    r"|max_tokens|temperature|embedding|gemini|ollama|bedrock|huggingface|rlm\.(infer|map|spawn)|tool_call|system_prompt",
    re.I,
)


def applies(reviewer: Reviewer, chunk: Chunk) -> bool:
    if chunk.kind == "doc":
        return reviewer.reads_docs
    if reviewer.llm_only:
        return bool(_LLM_HINT.search(chunk.text))
    return True


def _estimate_tokens(*texts: str, output: int) -> int:
    return sum(len(text) for text in texts) // 3 + output


def plan_find(chunks: list[Chunk], reviewers: list[Reviewer], budget_tokens: int) -> dict[str, Any]:
    """Pick (reviewer, chunk) frames that fit the find budget: code first, then tests, then docs."""
    order = {"code": 0, "test": 1, "doc": 2}
    frames: list[tuple[Reviewer, Chunk]] = []
    dropped: list[tuple[Reviewer, Chunk]] = []
    not_applicable: dict[str, int] = {}
    spent = 0
    for chunk in sorted(chunks, key=lambda item: (order[item.kind], item.id)):
        for reviewer in reviewers:
            if not applies(reviewer, chunk):
                not_applicable[reviewer.key] = not_applicable.get(reviewer.key, 0) + 1
                continue
            cost = _estimate_tokens(finder_task(reviewer), chunk.text, output=1_500)
            if spent + cost > budget_tokens:
                dropped.append((reviewer, chunk))
                continue
            spent += cost
            frames.append((reviewer, chunk))
    # Group by reviewer so frames sharing a task run together (their prompts share a cacheable prefix).
    keys = list(REVIEWERS)
    frames.sort(key=lambda pair: (keys.index(pair[0].key), pair[1].id))
    return {"frames": frames, "dropped": dropped, "not_applicable": not_applicable, "estimate": spent}


# --- Findings --------------------------------------------------------------------------------------------------

_CATEGORY_WORDS: dict[str, tuple[str, ...]] = {
    "security": ("secur", "inject", "auth", "secret", "credential", "xss", "ssrf", "travers", "crypto", "vuln",
                 "permission", "privilege", "sanitiz"),
    "ai": ("prompt", "llm", "model output", "token limit", "max_tokens", "hallucin"),
    "tests": ("test", "coverage", "flak", "assert", "regression"),
    "maintainability": ("arch", "design", "maint", "duplic", "coupl", "compat", "complex", "structure", "layer",
                        "public api", "breaking", "readab", "dead code"),
    "correctness": ("bug", "logic", "correct", "error", "exception", "null", "none", "race", "async", "concurr",
                    "bound", "off-by", "resource", "leak", "crash", "wrong"),
}
_REVIEWER_CATEGORY = {"bugs": "correctness", "security": "security", "arch": "maintainability", "tests": "tests",
                      "ai": "ai"}


def normalize_category(category: str, reviewer: str | None = None) -> str:
    text = (category or "").strip().lower()
    for key, words in _CATEGORY_WORDS.items():
        if text == key or any(word in text for word in words):
            return key
    return _REVIEWER_CATEGORY.get(reviewer or "", text or "other")


def _text(value: Any, limit: int) -> str:
    text = value if isinstance(value, str) else ("" if value is None else str(value))
    text = " ".join(text.split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def normalize_findings(raw: Any, reviewer: Reviewer, chunk: Chunk, line_count: int | None) -> list[dict[str, Any]]:
    """Validate a finder frame's reply: pin the file to the chunk's, keep lines in range, bound every string."""
    if not isinstance(raw, list):
        return []
    lo = min(hunk.new_first for hunk in chunk.hunks) if chunk.hunks else 1
    hi = max(hunk.new_last for hunk in chunk.hunks) if chunk.hunks else 1
    out = []
    for item in raw[:MAX_FINDINGS_PER_FRAME]:
        if not isinstance(item, dict):
            continue
        claim = _text(item.get("claim"), 300)
        if not claim:
            continue
        try:
            line = int(item.get("line"))
        except (TypeError, ValueError):
            line = lo
        upper = line_count if line_count else hi + CONTEXT_LINES
        if not 1 <= line <= max(upper, 1):
            line = lo
        severity = str(item.get("severity", "")).strip().lower()
        try:
            confidence = float(item.get("confidence", 0.5))
        except (TypeError, ValueError):
            confidence = 0.5
        out.append({
            # A frame saw one file; a different path in its reply is a slip, not a second file.
            "file": chunk.path,
            "line": line,
            "severity": severity if severity in SEVERITIES else "minor",
            "category": normalize_category(_text(item.get("category"), 40), reviewer.key),
            "claim": claim,
            "why": _text(item.get("why"), 600),
            "suggested_fix": _text(item.get("suggested_fix"), 500),
            "confidence": min(1.0, max(0.0, confidence)),
            "reviewers": [reviewer.key],
            "chunk": chunk.id,
        })
    return out


_STOP = {"the", "and", "for", "with", "that", "this", "when", "its", "has", "have", "are", "was", "not", "but", "from",
         "into", "than", "then", "which", "can", "will", "does", "any"}


def _words(text: str) -> set[str]:
    return {word for word in re.findall(r"[a-z0-9_]+", text.lower()) if len(word) > 2 and word not in _STOP}


def similar_claims(a: str, b: str) -> bool:
    """Two claims about the same problem in different words: at least half of the shorter claim's words recur."""
    left, right = _words(a), _words(b)
    if len(left) < 3 or len(right) < 3:
        return False
    return len(left & right) / min(len(left), len(right)) >= 0.5


def _rank(finding: dict[str, Any]) -> tuple[int, float]:
    return (SEVERITIES.index(finding["severity"]), -finding["confidence"])


def dedupe(findings: list[dict[str, Any]], window: int = DEDUPE_WINDOW) -> list[dict[str, Any]]:
    """Merge findings on the same file within `window` lines that share a category or make the same claim.
    The merged finding keeps the most severe (then most confident) wording and every reviewer that raised it."""
    merged: list[dict[str, Any]] = []
    for finding in sorted(findings, key=lambda item: (item["file"], item["line"], _rank(item))):
        for group in merged:
            if group["file"] != finding["file"] or abs(group["line"] - finding["line"]) > window:
                continue
            if group["category"] != finding["category"] and not similar_claims(group["claim"], finding["claim"]):
                continue
            reviewers = group["reviewers"] + [key for key in finding["reviewers"] if key not in group["reviewers"]]
            duplicates = group.get("duplicates", 1) + 1
            if _rank(finding) < _rank(group):
                group.clear()
                group.update(finding)
            group["reviewers"] = reviewers
            group["duplicates"] = duplicates
            group["confidence"] = max(group["confidence"], finding["confidence"])
            break
        else:
            merged.append(dict(finding, reviewers=list(finding["reviewers"])))
    merged.sort(key=_rank)
    for number, finding in enumerate(merged, 1):
        finding["id"] = number
    return merged


# --- Verification ----------------------------------------------------------------------------------------------

_DEF = re.compile(r"^\s*(?:export\s+)?(?:async\s+)?(?:def|function|class|func|fn)\s+([A-Za-z_]\w*)"
                  r"|^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_]\w*)\s*=\s*(?:async\s*)?(?:function|\()")
_CALL = re.compile(r"\b([A-Za-z_]\w{2,})\s*\(")
_COMMON = {
    "if", "for", "while", "return", "print", "len", "range", "str", "int", "float", "list", "dict", "set", "tuple",
    "isinstance", "super", "self", "this", "new", "await", "async", "function", "typeof", "catch", "switch",
    "open", "map", "filter", "sorted", "min", "max", "sum", "any", "all", "enumerate", "zip", "format", "append",
    "push", "get", "join", "split", "strip", "require", "console", "log", "Error", "Exception", "assert", "expect",
}


def symbols_for(source: list[str] | None, line: int, limit: int = 3) -> list[str]:
    """Names worth searching for around a cited line: its enclosing definition and what the line calls."""
    if not source:
        return []
    names: list[str] = []
    for index in range(min(line, len(source)) - 1, max(-1, line - 80), -1):
        match = _DEF.match(source[index])
        if match:
            names.append(match.group(1) or match.group(2))
            break
    if 1 <= line <= len(source):
        for name in _CALL.findall(source[line - 1]):
            if name not in _COMMON and name not in names:
                names.append(name)
    return names[:limit]


def source_window(source: list[str] | None, line: int, radius: int = VERIFY_WINDOW) -> str:
    if not source:
        return "(source unavailable)"
    start, end = max(1, line - radius), min(len(source), line + radius)
    return "\n".join(f"{'>' if number == line else ' '}{number:>5} | {_clip(source[number - 1])}"
                     for number in range(start, end + 1))


def hunk_for(item: FileDiff | None, line: int) -> Hunk | None:
    if item is None or not item.hunks:
        return None
    return min(item.hunks, key=lambda hunk: 0 if hunk.new_first <= line <= hunk.new_last
               else min(abs(hunk.new_first - line), abs(hunk.new_last - line)))


def _normalize(text: str) -> str:
    return " ".join(text.split())


_WINDOW_LINE = re.compile(r"^[> ]\s*\d+ \| ?(.*)$")
_HUNK_LINE = re.compile(r"^[ \d]{5} [+\- ] (.*)$")


def _source_bodies(source_text: str) -> list[str]:
    """The code on each line of a rendered source window or hunk, without gutters."""
    bodies = []
    for line in source_text.splitlines():
        match = _WINDOW_LINE.match(line) or _HUNK_LINE.match(line)
        bodies.append(_normalize(match.group(1) if match else line))
    return bodies


def evidence_quotes_source(evidence: str, source_text: str) -> bool:
    """True when the evidence quotes the source: a source line (6+ chars) appears in it, or a quoted or
    backticked snippet (6+ chars) from it appears in the source."""
    evidence_n = _normalize(evidence)
    bodies = _source_bodies(source_text)
    if not evidence_n or not bodies:
        return False
    for body in bodies:
        if len(body) >= 6 and re.search(r"[A-Za-z0-9]", body) and body in evidence_n:
            return True
    source_n = "\n".join(bodies)
    for snippet in re.findall(r"`([^`]{6,})`|\"([^\"]{6,})\"|'([^']{6,})'", evidence):
        text = _normalize(next(part for part in snippet if part))
        if text and text in source_n:
            return True
    return False


def apply_verdicts(findings: list[dict[str, Any]], verdicts: list[Any], sources: list[str],
                   line_counts: list[int | None]) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    """Split verified findings into (confirmed, uncertain, rejected). Rejected findings never reach the report."""
    confirmed, uncertain, rejected = [], [], []
    for finding, verdict, source, count in zip(findings, verdicts, sources, line_counts):
        finding = dict(finding)
        if isinstance(verdict, (Incomplete, FrameError)) or not isinstance(verdict, dict):
            status = getattr(verdict, "status", None) or getattr(verdict, "error", None) or "no verdict"
            finding["verification"] = f"not verified ({_text(status, 120)})"
            uncertain.append(finding)
            continue
        kind = verdict.get("verdict")
        evidence = _text(verdict.get("evidence"), 500)
        finding["evidence"] = evidence
        corrected = verdict.get("corrected_line")
        if isinstance(corrected, int) and not isinstance(corrected, bool) and corrected >= 1 and (
                count is None or corrected <= count):
            finding["line"] = corrected
        if kind == "rejected":
            rejected.append(finding)
        elif kind == "confirmed" and evidence_quotes_source(evidence, source):
            confirmed.append(finding)
        elif kind == "confirmed":
            finding["verification"] = "the verifier confirmed it without quoting the source"
            uncertain.append(finding)
        else:
            finding["verification"] = evidence or "the verifier could not decide"
            uncertain.append(finding)
    return confirmed, uncertain, rejected


def parse_json_object(text: Any) -> dict[str, Any] | None:
    """The last JSON object in a sub-agent's reply."""
    if isinstance(text, dict):
        return text
    if not isinstance(text, str):
        return None
    decoder = json.JSONDecoder()
    found = None
    for match in re.finditer(r"\{", text):
        try:
            value, _ = decoder.raw_decode(text[match.start():])
        except ValueError:
            continue
        if isinstance(value, dict) and "verdict" in value:
            found = value
    return found


# --- Scope -----------------------------------------------------------------------------------------------------


@dataclass
class Scope:
    label: str
    files: list[FileDiff]
    read_file: Callable[[str], list[str] | None]
    root: str = "."
    grep_rev: str | None = None  # None: search the working tree; a commit: search it; "" : no search
    pr: int | None = None
    pr_url: str | None = None
    notes: list[str] = field(default_factory=list)


def default_branch(git: Git) -> str | None:
    head = git.value("symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD")
    if head:
        return head
    for name in ("main", "master", "trunk", "develop", "origin/main", "origin/master"):
        ref = f"refs/heads/{name}" if "/" not in name else f"refs/remotes/{name}"
        if git.ok("rev-parse", "--verify", "--quiet", ref):
            return name
    return None


def _local_reader(cwd: str) -> Callable[[str], list[str] | None]:
    cache: dict[str, list[str] | None] = {}
    root = Path(cwd).resolve()

    def read(path: str) -> list[str] | None:
        if path not in cache:
            target = (root / path).resolve()
            try:
                target.relative_to(root)
                cache[path] = target.read_text(encoding="utf-8", errors="replace").splitlines()
            except (OSError, ValueError):
                cache[path] = None
        return cache[path]

    return read


def _rev_reader(git: Git, rev: str) -> Callable[[str], list[str] | None]:
    cache: dict[str, list[str] | None] = {}

    def read(path: str) -> list[str] | None:
        if path not in cache:
            code, stdout, _ = git.call("show", f"{rev}:{path}")
            cache[path] = stdout.splitlines() if code == 0 else None
        return cache[path]

    return read


def _untracked(git: Git, cwd: str, paths: list[str]) -> tuple[list[FileDiff], list[str]]:
    code, stdout, _ = git.call("ls-files", "--others", "--exclude-standard", "-z", "--", *paths)
    if code != 0:
        return [], []
    names = [name for name in stdout.split("\0") if name]
    files, notes = [], []
    for name in names[:MAX_UNTRACKED_FILES]:
        target = Path(cwd, name)
        try:
            data = target.read_bytes()
        except OSError:
            continue
        if len(data) > MAX_UNTRACKED_BYTES or b"\0" in data[:8192]:
            notes.append(f"untracked {name} (binary or over {MAX_UNTRACKED_BYTES // 1000} KB)")
            continue
        files.append(added_file(name, data.decode("utf-8", "replace")))
    if len(names) > MAX_UNTRACKED_FILES:
        notes.append(f"{len(names) - MAX_UNTRACKED_FILES} more untracked files (only the first {MAX_UNTRACKED_FILES} were read)")
    return files, notes


def _within(path: str, prefixes: list[str]) -> bool:
    return not prefixes or any(path == prefix.rstrip("/") or path.startswith(prefix.rstrip("/") + "/") for prefix in prefixes)


def resolve_scope(options: Options, cwd: str, *, runner: Runner | None = None,
                  which: Callable[[str], str | None] = shutil.which) -> Scope:
    git = Git(cwd, runner)
    if not git.ok("rev-parse", "--git-dir"):
        raise ReviewError(f"{cwd} is not inside a git repository, so there is no diff to review")
    top = git.value("rev-parse", "--show-toplevel") or cwd
    git = Git(top, runner)
    prefix = os.path.relpath(cwd, top) if os.path.abspath(cwd) != os.path.abspath(top) else ""

    def exists(token: str) -> bool:
        return Path(cwd, token).exists()

    def is_ref(token: str) -> bool:
        return not token.startswith("-") and git.ok("rev-parse", "--verify", "--quiet", f"{token}^{{commit}}")

    target = classify_targets(options, exists=exists, is_ref=is_ref)
    # Paths are given relative to cwd; git diff paths are relative to the top level.
    paths = [os.path.normpath(os.path.join(prefix, path)) if prefix else os.path.normpath(path) for path in target.paths]
    paths = ["" if path == "." else path for path in paths]
    paths = [path for path in paths if path] if not any(path == "" for path in paths) else []
    where = f" in {', '.join(target.paths)}" if target.paths else ""

    if target.pr is not None:
        if which("gh") is None:
            raise ReviewError("reviewing a pull request needs the GitHub CLI (gh), which is not installed")
        code, diff, stderr = (runner or _run_process)(["gh", "pr", "diff", str(target.pr), "--color=never"], top,
                                                      GH_TIMEOUT_S)
        if code != 0:
            raise ReviewError(f"gh pr diff {target.pr} failed: {(stderr or diff).strip()[:300]}")
        meta: dict[str, Any] = {}
        code, stdout, _ = (runner or _run_process)(
            ["gh", "pr", "view", str(target.pr), "--json", "number,title,url,headRefOid,baseRefName"], top, GH_TIMEOUT_S)
        if code == 0:
            try:
                meta = json.loads(stdout)
            except ValueError:
                meta = {}
        files = [item for item in parse_diff(diff) if _within(item.path, paths)]
        head = meta.get("headRefOid")
        notes = []
        if head and git.value("rev-parse", "HEAD") == head:
            reader, grep_rev = _local_reader(top), None
        elif head and git.ok("cat-file", "-e", f"{head}^{{commit}}"):
            reader, grep_rev = _rev_reader(git, head), head
        else:
            reader, grep_rev = (lambda _path: None), ""
            notes.append("code around the diff and callers (the PR head commit is not in this clone; fetch it or "
                         "check out the PR branch to include them)")
        title = f": {meta['title']}" if meta.get("title") else ""
        label = f"PR #{target.pr}{title}{where}"
        return Scope(label, files, reader, top, grep_rev, target.pr, meta.get("url"), notes)

    has_head = git.ok("rev-parse", "--verify", "--quiet", "HEAD")
    branch = git.value("branch", "--show-current") or "HEAD"
    if target.base is not None:
        base = git.value("merge-base", target.base, "HEAD") if has_head else None
        base = base or git.out("rev-parse", "--verify", f"{target.base}^{{commit}}").strip()
        label = f"changes since {target.base} (merge base {base[:8]}), including uncommitted{where}"
    elif not has_head:
        base, label = EMPTY_TREE, f"all files (the repository has no commits yet){where}"
    else:
        default = default_branch(git)
        merge_base = git.value("merge-base", default, "HEAD") if default else None
        head = git.value("rev-parse", "HEAD")
        if merge_base and merge_base != head:
            base = merge_base
            label = f"branch {branch} vs {default} (merge base {merge_base[:8]}) plus uncommitted changes{where}"
        else:
            base, label = "HEAD", f"uncommitted changes on {branch}{where}"
    diff = git.out("-c", "diff.noprefix=false", "diff", "--no-color", "--no-ext-diff", "--no-relative",
                   "--src-prefix=a/", "--dst-prefix=b/", "-M", "-U3", base, "--", *paths)
    files = parse_diff(diff)
    untracked, notes = _untracked(git, top, paths)
    files += untracked
    return Scope(label, files, _local_reader(top), top, None, notes=[f"skipped {note}" for note in notes])


def related_code(git: Git, scope: Scope, finding: dict[str, Any], source: list[str] | None) -> str:
    """Where the names around a finding are defined or used elsewhere (git grep), bounded."""
    if scope.grep_rev == "":
        return ""
    lines: list[str] = []
    size = 0
    for name in symbols_for(source, finding["line"]):
        args = ["grep", "-n", "-w", "-I", "--max-count=4", "-e", name]
        args += [scope.grep_rev] if scope.grep_rev else ["--untracked"]
        code, stdout, _ = git.call(*args, "--", ".", ":(exclude)*.lock", ":(exclude)*.min.js")
        if code != 0:
            continue
        for hit in stdout.splitlines()[:8]:
            if scope.grep_rev and hit.startswith(scope.grep_rev + ":"):
                hit = hit[len(scope.grep_rev) + 1:]
            path, _, rest = hit.partition(":")
            number, _, text = rest.partition(":")
            if path == finding["file"] and number.isdigit() and abs(int(number) - finding["line"]) <= VERIFY_WINDOW:
                continue
            entry = f"{path}:{number}: {_clip(text.strip(), 200)}"
            if size + len(entry) > RELATED_CHARS:
                break
            lines.append(entry)
            size += len(entry)
    return "\n".join(lines)


# --- Report ----------------------------------------------------------------------------------------------------


@dataclass
class Review:
    report: str
    path: str | None = None
    scope: str = ""
    confirmed: list[dict[str, Any]] = field(default_factory=list)
    uncertain: list[dict[str, Any]] = field(default_factory=list)
    rejected: list[dict[str, Any]] = field(default_factory=list)
    stats: dict[str, Any] = field(default_factory=dict)
    pr: int | None = None
    post_pending: bool = False

    def __repr__(self) -> str:
        return (f"Review({len(self.confirmed)} confirmed, {len(self.uncertain)} uncertain, {len(self.rejected)} "
                f"rejected; report: {self.path or 'not saved'})")

    def __str__(self) -> str:
        return self.report


def _quote(read_file: Callable[[str], list[str] | None], finding: dict[str, Any]) -> str:
    source = read_file(finding["file"])
    line = finding["line"]
    if not source or not 1 <= line <= len(source):
        return ""
    return f"    {line} | {_clip(source[line - 1].rstrip(), 200)}"


def render_report(*, label: str, files: list[FileDiff], reviewers: list[Reviewer], confirmed: list[dict[str, Any]],
                  uncertain: list[dict[str, Any]], rejected: list[dict[str, Any]], stats: dict[str, Any],
                  not_checked: list[str], read_file: Callable[[str], list[str] | None],
                  path: str | None = None, post_line: str | None = None) -> str:
    """Markdown report: confirmed findings by severity, uncertain ones apart, then counts, cost and gaps.
    Rejected findings contribute only to the counts."""
    added = sum(item.added for item in files)
    removed = sum(item.removed for item in files)
    out = [f"# Code review: {label}", ""]
    out.append(f"{len(files)} files, +{added} -{removed} lines. Reviewers: {', '.join(r.title for r in reviewers)}.")
    if not confirmed:
        out += ["", "No confirmed findings."]
    shown = 0
    for severity in SEVERITIES:
        group = [finding for finding in confirmed if finding["severity"] == severity]
        if not group:
            continue
        out += ["", f"## {SEVERITY_TITLES[severity]} ({len(group)})"]
        for finding in group:
            shown += 1
            where = f"{finding['file']}:{finding['line']}"
            if shown > REPORT_FINDINGS:
                out += ["", f"- `{where}` {finding['claim']}"]
                continue
            by = ", ".join(finding["reviewers"])
            out += ["", f"**{shown}. `{where}`** {finding['claim']}",
                    f"_{finding['category']}; raised by {by}; confidence {finding['confidence']:.1f}_"]
            quote = _quote(read_file, finding)
            if quote:
                out += ["", quote, ""]
            if finding.get("evidence"):
                out.append(f"- Evidence: {finding['evidence']}")
            if finding.get("why"):
                out.append(f"- Why: {finding['why']}")
            if finding.get("suggested_fix"):
                out.append(f"- Fix: {finding['suggested_fix']}")
    if uncertain:
        out += ["", f"## Uncertain, not confirmed ({len(uncertain)})", ""]
        for finding in uncertain[:REPORT_FINDINGS]:
            note = finding.get("verification") or ""
            out.append(f"- `{finding['file']}:{finding['line']}` ({finding['severity']}) {finding['claim']}"
                       + (f" Verifier: {note}" if note else ""))
        if len(uncertain) > REPORT_FINDINGS:
            out.append(f"- and {len(uncertain) - REPORT_FINDINGS} more in the saved report")
    out += ["", "## Summary", ""]
    out.append(f"- Findings: {stats.get('raised', 0)} raised, {stats.get('merged', 0)} after merging duplicates, "
               f"{len(confirmed)} confirmed, {len(rejected)} rejected, {len(uncertain)} uncertain.")
    tokens = stats.get("tokens", 0)
    cost = (f"- Cost: {stats.get('frames', 0)} model calls for {stats.get('find_frames', 0)} finder and "
            f"{stats.get('verify_frames', 0)} verifier frames (re-asks included), {tokens:,} tokens of a "
            f"{stats.get('budget', 0):,}-token cap")
    if stats.get("subagents"):
        cost += f"; {stats['subagents']} verification sub-agents (their tokens count in the session usage)"
    out.append(cost + ".")
    if path:
        out.append(f"- Full report: {path}")
    if post_line:
        out.append(f"- {post_line}")
    out += ["", "### What this review did not check", ""]
    gaps = ["Runtime behavior: nothing was executed (no tests, build or type check); findings come from reading the "
            "diff, the code around it and name-matched callers.",
            "Effects on code that does not mention the changed names (dynamic dispatch, reflection, other services)."]
    out += [f"- {gap}" for gap in gaps + not_checked]
    return "\n".join(out) + "\n"


def _save(report: str, git: Git | None) -> str | None:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    directory = None
    if git is not None:
        git_dir = git.value("rev-parse", "--absolute-git-dir")
        if git_dir:
            directory = Path(git_dir, "ultron-review")
    if directory is None:
        directory = Path(tempfile.gettempdir(), "ultron-review")
    try:
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / f"review-{stamp}.md"
        path.write_text(report, encoding="utf-8")
        return str(path)
    except OSError:
        return None


# --- Orchestration ---------------------------------------------------------------------------------------------


def _spent(results: Any) -> tuple[int, int]:
    spent = getattr(results, "spent", None) or {}
    return int(spent.get("calls") or 0), int(spent.get("tokens") or 0)


def _failure(result: Any) -> str | None:
    if isinstance(result, Incomplete):
        return f"ran out ({result.status})"
    if isinstance(result, FrameError):
        return f"failed ({_text(result.error, 120)})"
    return None


def _plan_report(label: str, chunks: list[Chunk], plan: dict[str, Any], skipped: list[tuple[str, str]],
                 options: Options) -> str:
    out = [f"# Review plan: {label}", "", f"{len(chunks)} chunks; {len(plan['frames'])} finder frames, about "
           f"{plan['estimate']:,} tokens of the {int(options.budget_tokens * FIND_SHARE):,}-token find budget.", ""]
    for chunk in chunks:
        keys = [reviewer.key for reviewer, item in plan["frames"] if item is chunk]
        part = f" part {chunk.part}/{chunk.parts}" if chunk.parts > 1 else ""
        out.append(f"- {chunk.path}{part} ({chunk.kind}, {len(chunk.text):,} chars): {', '.join(keys) or 'none'}")
    for path, reason in skipped:
        out.append(f"- skipped {path}: {reason}")
    if plan["dropped"]:
        out.append(f"- over budget: {len(plan['dropped'])} frames would not run")
    return "\n".join(out) + "\n"


async def _saved_review_model(rlm: Any) -> str | None:
    """`review.model` as the host reads it now (a /settings change applies to the next review); None when unset,
    so the frames take `rlm.frameModel` or the session's model from the host."""
    models = getattr(rlm, "_models", None)
    if not callable(models):
        return None
    try:
        review = (await models()).get("review") or {}
    except Exception:
        return None
    model = review.get("model")
    return model if review.get("source") == "setting" and isinstance(model, str) else None


async def run(rlm: Any, args: str | list[str] | None = "", *, cwd: str | None = None, runner: Runner | None = None,
              which: Callable[[str], str | None] = shutil.which) -> Review:
    """Review the changes `args` selects (see USAGE) and return a Review; `print(review.report)` shows it."""
    cwd = cwd or os.getcwd()
    try:
        options = parse_args(args, dict(os.environ))
    except ReviewError as error:
        return Review(f"/review: {error}\n")
    if options.help:
        return Review(USAGE + "\n")
    if options.model is None:
        options.frame_model = await _saved_review_model(rlm)
    try:
        scope = resolve_scope(options, cwd, runner=runner, which=which)
    except ReviewError as error:
        return Review(f"/review: {error}\n")
    git = Git(scope.root, runner)
    reviewers = options.reviewers
    chunks, skipped = build_chunks(scope.files, scope.read_file)
    not_checked = list(scope.notes)
    not_checked += [f"{path}: {reason}" for path, reason in skipped]
    left_out = [reviewer.title for key, reviewer in REVIEWERS.items() if reviewer not in reviewers]
    if left_out:
        not_checked.append(f"Reviewers not selected: {', '.join(left_out)}.")
    if not chunks:
        reason = "No changes to review" if not scope.files else "No reviewable changes"
        report = f"# Code review: {scope.label}\n\n{reason}.\n"
        if skipped:
            report += "\n" + "\n".join(f"- skipped {path}: {why}" for path, why in skipped) + "\n"
        return Review(report, scope=scope.label, pr=scope.pr)

    find_budget = int(options.budget_tokens * FIND_SHARE)
    plan = plan_find(chunks, reviewers, find_budget)
    if options.plan:
        return Review(_plan_report(scope.label, chunks, plan, skipped, options), scope=scope.label, pr=scope.pr)
    for reviewer_key, count in plan["not_applicable"].items():
        reviewer = REVIEWERS[reviewer_key]
        why = "no LLM-related code" if reviewer.llm_only else "documentation only"
        not_checked.append(f"{reviewer.title} reviewer skipped {count} chunk(s) with {why}.")
    if plan["dropped"]:
        paths = sorted({chunk.path for _, chunk in plan["dropped"]})
        not_checked.append(f"{len(plan['dropped'])} reviewer passes did not fit the budget (raise --budget): "
                           + ", ".join(paths[:10]) + (" ..." if len(paths) > 10 else ""))

    stats: dict[str, Any] = {"budget": options.budget_tokens, "frames": 0, "tokens": 0, "find_frames": 0,
                             "verify_frames": 0, "subagents": 0}
    frames = plan["frames"]
    raised: list[dict[str, Any]] = []
    if frames:
        results = await rlm.map([finder_task(reviewer) for reviewer, _ in frames], [chunk.text for _, chunk in frames],
                                contract=FINDINGS_CONTRACT, budget=Budget(tokens=find_budget),
                                model=options.model or options.frame_model,
                                timeout_ms=FRAME_TIMEOUT_MS)
        calls, tokens = _spent(results)
        stats.update(find_frames=len(frames), frames=calls, tokens=tokens)
        failures: dict[str, list[str]] = {}
        for (reviewer, chunk), result in zip(frames, results):
            failure = _failure(result)
            if failure:
                failures.setdefault(failure, []).append(f"{reviewer.key} on {chunk.path}")
                continue
            source = scope.read_file(chunk.path)
            raised += normalize_findings(result, reviewer, chunk, len(source) if source else None)
        for failure, where in failures.items():
            not_checked.append(f"{len(where)} reviewer passes {failure}: " + ", ".join(where[:8])
                               + (" ..." if len(where) > 8 else ""))
    stats["raised"] = len(raised)
    merged = dedupe(raised)
    stats["merged"] = len(merged)

    # Verify within what the find phase left of the cap, most severe first.
    verify_budget = max(0, options.budget_tokens - stats["tokens"])
    by_path = {item.path: item for item in scope.files}
    items: list[list[str]] = []
    sources: list[str] = []
    counts: list[int | None] = []
    to_verify: list[dict[str, Any]] = []
    unverified: list[dict[str, Any]] = []
    estimate = 0
    for finding in merged:
        source = scope.read_file(finding["file"])
        window = source_window(source, finding["line"])
        hunk = hunk_for(by_path.get(finding["file"]), finding["line"])
        hunk_text = "\n".join(render_hunk(hunk)) if hunk else "(no hunk)"
        related = related_code(git, scope, finding, source)
        public = {key: finding[key] for key in ("file", "line", "severity", "category", "claim", "why", "suggested_fix")}
        views = [f"Finding:\n{json.dumps(public, indent=1)}",
                 f"Source of {finding['file']} around line {finding['line']} (> marks the cited line):\n{window}",
                 f"Diff hunk ({finding['file']}, new-file line numbers):\n{hunk_text}"]
        if related:
            views.append(f"Other places that define or use the names involved:\n{related}")
        cost = _estimate_tokens(VERIFIER_TASK, *views, output=600)
        if estimate + cost > verify_budget:
            unverified.append(dict(finding, verification="not verified (budget)"))
            continue
        estimate += cost
        items.append(views)
        sources.append(window + "\n" + hunk_text)
        counts.append(len(source) if source else None)
        to_verify.append(finding)
    confirmed: list[dict[str, Any]] = []
    uncertain: list[dict[str, Any]] = []
    rejected: list[dict[str, Any]] = []
    if to_verify:
        verdicts = await rlm.map(VERIFIER_TASK, items, contract=VERDICT_CONTRACT, budget=Budget(tokens=verify_budget),
                                 model=options.model or options.frame_model, timeout_ms=FRAME_TIMEOUT_MS)
        calls, tokens = _spent(verdicts)
        stats["verify_frames"] = len(to_verify)
        stats["frames"] += calls
        stats["tokens"] += tokens
        confirmed, uncertain, rejected = apply_verdicts(to_verify, list(verdicts), sources, counts)
    uncertain += unverified
    if unverified:
        not_checked.append(f"{len(unverified)} findings were not verified within the budget (listed as uncertain).")

    if options.deep and uncertain:
        confirmed, uncertain, rejected, spawned = await _deep_verify(rlm, options, cwd, scope, confirmed, uncertain,
                                                                      rejected)
        stats["subagents"] = spawned

    confirmed.sort(key=_rank)
    post_line = None
    post_pending = False
    if options.post:
        if scope.pr is None:
            post_line = "Not posted: --post needs a pull request (pass its number)."
        else:
            post_pending = True
            post_line = (f"Post pending: nothing has been posted to PR #{scope.pr}. It is posted only after the "
                         f"user explicitly confirms.")
    report_args = dict(label=scope.label, files=scope.files, reviewers=reviewers, confirmed=confirmed,
                       uncertain=uncertain, rejected=rejected, stats=stats, not_checked=not_checked,
                       read_file=scope.read_file, post_line=post_line)
    path = _save(render_report(**report_args), git)
    report = render_report(**report_args, path=path)
    if path:
        try:
            Path(path).write_text(report, encoding="utf-8")
        except OSError:
            pass
    return Review(report, path, scope.label, confirmed, uncertain, rejected, stats, scope.pr, post_pending)


async def _deep_verify(rlm: Any, options: Options, cwd: str, scope: Scope, confirmed: list[dict[str, Any]],
                       uncertain: list[dict[str, Any]], rejected: list[dict[str, Any]]) -> tuple[Any, Any, Any, int]:
    """Re-check the most severe uncertain findings with sub-agents that can read the whole repository."""
    chosen = sorted(uncertain, key=_rank)[: options.deep]
    rest = [finding for finding in uncertain if finding not in chosen]
    handles = []
    for index, finding in enumerate(chosen, 1):
        public = {key: finding[key] for key in ("file", "line", "severity", "category", "claim", "why")}
        brief = DEEP_VERIFY_BRIEF.format(cwd=cwd, finding=json.dumps(public, indent=1),
                                         note=finding.get("verification") or "no verdict")
        kwargs: dict[str, Any] = {"name": f"review-check-{index}", "timeout_ms": DEEP_TIMEOUT_MS}
        if options.model:
            kwargs["model"] = options.model
        handles.append(await rlm.spawn(brief, **kwargs))
    results = await rlm.collect(handles, timeout_ms=DEEP_TIMEOUT_MS + 30_000)
    by_id = {entry.get("id"): entry.get("result") or {} for entry in results if isinstance(entry, dict)}
    verdicts = []
    for handle in handles:
        result = by_id.get(getattr(handle, "rlm_child_id", handle), {})
        value = parse_json_object(result.get("value")) if result.get("status") == "succeeded" else None
        verdicts.append(value)
    sources = []
    counts = []
    for finding in chosen:
        source = scope.read_file(finding["file"])
        sources.append("\n".join(source) if source else "")
        counts.append(len(source) if source else None)
    more_confirmed, still_uncertain, more_rejected = apply_verdicts(chosen, verdicts, sources, counts)
    return confirmed + more_confirmed, rest + still_uncertain, rejected + more_rejected, len(handles)


async def post(review: Review | str, *, confirm: bool = False, pr: int | None = None, runner: Runner | None = None,
               which: Callable[[str], str | None] = shutil.which, cwd: str | None = None) -> str:
    """Post a review's saved report as a PR comment with `gh`. Call only after the user explicitly said yes.
    `review` is a Review, or the saved report's path together with `pr=`."""
    if confirm is not True:
        raise ReviewError("posting needs confirm=True, and only after the user explicitly agreed to post")
    path = review if isinstance(review, str) else review.path
    number = pr if pr is not None else (None if isinstance(review, str) else review.pr)
    if number is None:
        raise ReviewError("this review has no pull request to post to")
    if not path or not Path(path).is_file():
        raise ReviewError("the review has no saved report to post")
    if which("gh") is None:
        raise ReviewError("posting needs the GitHub CLI (gh)")
    code, stdout, stderr = (runner or _run_process)(
        ["gh", "pr", "comment", str(number), "--body-file", path], cwd or os.getcwd(), GH_TIMEOUT_S)
    if code != 0:
        raise ReviewError(f"gh pr comment failed: {(stderr or stdout).strip()[:300]}")
    if isinstance(review, Review):
        review.post_pending = False
    return stdout.strip() or f"posted to PR #{number}"
