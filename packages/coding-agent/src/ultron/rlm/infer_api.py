"""Bounded inference: context handles (`rlm.load`, `rlm.open`) and inference frames (`rlm.infer`, `rlm.map`).

A handle interns a large input in this kernel and on disk (content-addressed under the session), and
never renders its content: only views (slices, line ranges, chunks) carry text, and only printing a view
puts text into the model's context. Frames are private, bounded sub-inferences the host runs as
`rlm-frame@1` tasks under a shared budget; they see only the views they are given.
"""
from __future__ import annotations

import hashlib
import os
import re
from array import array
from pathlib import Path
from typing import Any

_PREVIEW = 80
_MAX_PATH_TEXT = 4096
# Inline text a frame request may carry before it goes by handle: the kernel protocol frame is 1 MiB.
_INLINE_BYTES = 512 * 1024


def _short(text: str, limit: int = _PREVIEW) -> str:
    text = text.replace("\n", "\\n")
    return text if len(text) <= limit else text[: limit - 1] + "…"


class Budget:
    """Budget of a frame subtree: provider calls, charged tokens (cache reads discounted), and nesting depth.
    None means unbounded (repairs are still capped by `max_repairs`)."""

    def __init__(self, calls: int | None = None, tokens: int | None = None, depth: int = 1) -> None:
        for name, item in (("calls", calls), ("tokens", tokens)):
            if item is not None and (type(item) is not int or item < 0):
                raise ValueError(f"Budget.{name} must be a nonnegative int or None")
        if type(depth) is not int or not 1 <= depth <= 4:
            raise ValueError("Budget.depth must be an int from 1 to 4")
        self.calls = calls
        self.tokens = tokens
        self.depth = depth

    def to_json(self) -> dict[str, Any]:
        return {"calls": self.calls, "tokens": self.tokens, "depth": self.depth}

    def __repr__(self) -> str:
        return f"Budget(calls={self.calls!r}, tokens={self.tokens!r}, depth={self.depth!r})"


class ContextView:
    """A character range of a handle. `str(view)` / `view.text` materialize it; `repr` stays bounded."""

    __slots__ = ("handle", "start", "end")

    def __init__(self, handle: "ContextHandle", start: int, end: int) -> None:
        self.handle = handle
        self.start = start
        self.end = end

    @property
    def text(self) -> str:
        return self.handle._text[self.start : self.end]

    @property
    def label(self) -> str:
        return f"{self.handle.label}[{self.start}:{self.end}]"

    def __len__(self) -> int:
        return self.end - self.start

    def __str__(self) -> str:
        return self.text

    def __repr__(self) -> str:
        return f"ContextView({self.label!r}, {len(self)} chars, head={_short(self.text[:_PREVIEW])!r})"

    def _wire(self) -> dict[str, Any]:
        return {
            "kind": "text",
            "label": self.label,
            "text": self.text,
            "digest": self.handle.digest,
            "start": self.start,
            "end": self.end,
        }


class ContextHandle:
    """Interned content. Printing a handle shows its label, size and digest, never the content."""

    def __init__(self, text: str, digest: str, label: str, size: int) -> None:
        self._text = text
        self.digest = digest
        self.label = label
        self.size = size
        self._line_starts: array | None = None

    def length(self) -> int:
        """Length in characters (slice offsets are characters)."""
        return len(self._text)

    def __len__(self) -> int:
        return len(self._text)

    def _clamp(self, start: int | None, end: int | None, limit: int) -> tuple[int, int]:
        # Negative bounds clamp to 0 (not Python's from-the-end meaning), so `h.lines(n - 20, n + 5)` is safe.
        start = 0 if start is None else start
        end = limit if end is None else end
        start = min(max(0, start), limit)
        end = min(max(start, end), limit)
        return start, end

    def slice(self, start: int | None = 0, end: int | None = None) -> ContextView:
        """Characters [start, end); bounds are clamped to the content."""
        start, end = self._clamp(start, end, len(self._text))
        return ContextView(self, start, end)

    def _lines(self) -> array:
        if self._line_starts is None:
            starts = array("q", [0])
            for match in re.finditer("\n", self._text):
                if match.end() < len(self._text):
                    starts.append(match.end())
            self._line_starts = starts
        return self._line_starts

    def line_count(self) -> int:
        return len(self._lines()) if self._text else 0

    def lines(self, start: int | None = 0, end: int | None = None) -> ContextView:
        """Lines [start, end), 0-based like Python slicing; bounds are clamped."""
        starts = self._lines()
        count = len(starts) if self._text else 0
        first, last = self._clamp(start, end, count)
        begin = starts[first] if first < count else len(self._text)
        stop = starts[last] if last < count else len(self._text)
        return ContextView(self, begin, stop)

    def line_of(self, offset: int) -> int:
        """0-based line containing a character offset."""
        starts = self._lines()
        low, high = 0, len(starts) - 1
        while low < high:
            middle = (low + high + 1) // 2
            if starts[middle] <= offset:
                low = middle
            else:
                high = middle - 1
        return low

    def search(self, pattern: str, limit: int = 20, flags: int = 0) -> list[dict[str, Any]]:
        """Regex matches, at most `limit`: [{start, end, line (0-based), text (the matching line, bounded)}]."""
        if type(limit) is not int or limit < 1:
            raise ValueError("limit must be a positive int")
        found: list[dict[str, Any]] = []
        for match in re.finditer(pattern, self._text, flags):
            line = self.line_of(match.start())
            view = self.lines(line, line + 1)
            found.append({
                "start": match.start(),
                "end": match.end(),
                "line": line,
                "text": _short(view.text.rstrip("\n"), 240),
            })
            if len(found) >= limit:
                break
        return found

    def count(self, pattern: str, flags: int = 0) -> int:
        """Number of regex matches, without materializing them."""
        return sum(1 for _ in re.finditer(pattern, self._text, flags))

    def chunks(self, size: int, overlap: int = 0) -> list[ContextView]:
        """Views of at most `size` characters that end on a line boundary when one exists in range."""
        if type(size) is not int or size < 1:
            raise ValueError("size must be a positive int")
        if type(overlap) is not int or overlap < 0 or overlap >= size:
            raise ValueError("overlap must be an int in [0, size)")
        text = self._text
        views: list[ContextView] = []
        position = 0
        while position < len(text):
            end = min(position + size, len(text))
            if end < len(text):
                cut = text.rfind("\n", position, end)
                if cut >= position and cut + 1 > position + overlap:
                    end = cut + 1
            views.append(ContextView(self, position, end))
            if end >= len(text):
                break
            position = max(end - overlap, position + 1)
        return views

    def __repr__(self) -> str:
        return f"ContextHandle(label={self.label!r}, chars={len(self._text)}, size={self.size}, digest={self.digest[:19]!r}…)"

    __str__ = __repr__

    def _wire(self) -> dict[str, Any]:
        return {"kind": "handle", "label": self.label, "digest": self.digest, "chars": len(self._text), "size": self.size}


class Incomplete:
    """A frame that ran out of budget or repairs: evidence, not an exception. Always falsy."""

    def __init__(self, observation: dict[str, Any]) -> None:
        self.status = observation.get("reason") or observation.get("status") or "incomplete"
        self.detail = observation.get("detail", "")
        self.spent = observation.get("spent", {})
        self.remaining = observation.get("remaining", {})
        self.trace_id = observation.get("trace_id")
        self.last_outputs = list(observation.get("last_outputs") or [])

    def __bool__(self) -> bool:
        return False

    def __repr__(self) -> str:
        last = _short(self.last_outputs[-1]) if self.last_outputs else None
        return (
            f"Incomplete(status={self.status!r}, spent={self.spent!r}, remaining={self.remaining!r}, "
            f"trace_id={self.trace_id!r}, last_output={last!r})"
        )


class FrameError(Exception):
    """A frame that failed (provider error, cancellation, bad context). `rlm.map` returns these in place;
    `rlm.infer` raises `InferenceError`."""

    def __init__(self, observation: dict[str, Any]) -> None:
        super().__init__(str(observation.get("error") or "inference frame failed"))
        self.error = str(observation.get("error") or "inference frame failed")
        self.trace_id = observation.get("trace_id")
        self.spent = observation.get("spent", {})
        self.last_outputs = list(observation.get("last_outputs") or [])

    def __bool__(self) -> bool:
        return False

    def __repr__(self) -> str:
        return f"FrameError({_short(self.error, 200)!r}, trace_id={self.trace_id!r})"


class InferenceError(FrameError):
    pass


_TYPE_SCHEMAS: dict[Any, dict[str, Any]] = {
    int: {"type": "integer"},
    float: {"type": "number"},
    str: {"type": "string"},
    bool: {"type": "boolean"},
    list: {"type": "array"},
    dict: {"type": "object"},
    type(None): {"type": "null"},
}


_TYPE_NAMES = {kind.__name__: kind for kind in _TYPE_SCHEMAS if kind is not type(None)}
_SCHEMA_KEYS = {"type", "properties", "items", "enum", "const", "anyOf", "oneOf", "allOf", "$ref", "not"}


def _schema(contract: Any) -> Any:
    if contract is None:
        return None
    if isinstance(contract, dict):
        typed = any(isinstance(value, type) or hasattr(value, "__origin__") for value in contract.values())
        if not contract or (_SCHEMA_KEYS & contract.keys() and not typed):
            return contract
        # {"field": type, ...} shorthand for an object whose fields are all required.
        return {"type": "object", "properties": {key: _schema(value) for key, value in contract.items()},
                "required": list(contract)}
    if isinstance(contract, type) and contract in _TYPE_SCHEMAS:
        return dict(_TYPE_SCHEMAS[contract])
    if callable(getattr(contract, "model_json_schema", None)):  # a pydantic model
        return contract.model_json_schema()
    dataclass_fields = getattr(contract, "__dataclass_fields__", None)
    fields = ({name: field.type for name, field in dataclass_fields.items()} if isinstance(dataclass_fields, dict)
              else getattr(contract, "__annotations__", None))
    if isinstance(contract, type) and isinstance(fields, dict) and fields:  # a dataclass or TypedDict
        return _schema({name: _TYPE_NAMES.get(kind, Any) if isinstance(kind, str) else kind
                        for name, kind in fields.items()})
    origin = getattr(contract, "__origin__", None)
    arguments = getattr(contract, "__args__", ())
    if origin is list and len(arguments) == 1:
        return {"type": "array", "items": _schema(arguments[0])}
    if origin is dict and len(arguments) == 2 and arguments[0] is str:
        return {"type": "object", "additionalProperties": _schema(arguments[1])}
    import typing

    if origin is typing.Literal:
        return {"enum": list(arguments)}
    if origin is typing.Union or type(contract).__name__ == "UnionType":  # Optional[T], T | None
        return {"anyOf": [_schema(argument) for argument in arguments]}
    if contract is Any:
        return {}
    raise TypeError("contract must be a JSON schema dict, a builtin type (int, str, float, bool, list, dict), "
                    "list[T], Literal[...], T | None, a dataclass or {'field': type}")


def _wire_item(item: Any, out: list[dict[str, Any]]) -> None:
    if isinstance(item, (ContextView, ContextHandle)):
        out.append(item._wire())
    elif isinstance(item, str):
        out.append({"kind": "text", "label": "literal", "text": item})
    elif isinstance(item, (list, tuple)):
        for nested in item:
            _wire_item(nested, out)
    elif item is None:
        return
    else:
        import json

        out.append({"kind": "text", "label": type(item).__name__, "text": json.dumps(item, default=str)})


def _wire_context(context: Any) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    _wire_item(context, out)
    return out


def _budget(budget: Any) -> dict[str, Any] | None:
    if budget is None:
        return None
    if isinstance(budget, Budget):
        return budget.to_json()
    if isinstance(budget, dict):
        return Budget(**budget).to_json()
    raise TypeError("budget must be a Budget(calls, tokens, depth)")


def _outcome(observation: dict[str, Any], raise_errors: bool) -> Any:
    status = observation.get("status")
    if status == "complete":
        return observation.get("value")
    if status == "incomplete":
        return Incomplete(observation)
    if raise_errors:
        raise InferenceError(observation)
    return FrameError(observation)


class MapResults(list):
    """`rlm.map`'s results in order, plus what the map spent: `.spent`, `.budget` (limits) and `.remaining`."""

    _snapshot_as_list = True
    spent: dict[str, Any]
    budget: dict[str, Any]
    remaining: dict[str, Any]

    def summary(self) -> str:
        complete = sum(1 for item in self if not isinstance(item, (Incomplete, FrameError)))
        incomplete = sum(1 for item in self if isinstance(item, Incomplete))
        failed = len(self) - complete - incomplete
        tokens = self.spent.get("tokens")
        limit = self.budget.get("tokens")
        spent = f"{self.spent.get('calls', 0)} calls, {tokens:,} tokens" if isinstance(tokens, int) else "spend unknown"
        of = f" of {limit:,}" if isinstance(limit, int) else ""
        return (f"[rlm.map] {len(self)} frames: {complete} complete, {incomplete} incomplete, {failed} failed; "
                f"spent {spent}{of}")


class Inference:
    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge
        self._handles: dict[str, ContextHandle] = {}

    async def load(self, source: Any = None, *, path: Any = None, text: str | None = None,
                   data: bytes | None = None, label: str | None = None) -> ContextHandle:
        """Intern a file (path), text, or bytes and return a ContextHandle. The content is never returned.

        A handle (`h.size`, `h.digest`; printing it never shows content) is programmed over, not read:
        `h.search(regex, limit=20)` -> [{start, end, line, text}], `h.count(regex)`, `h.lines(a, b)` (0-based,
        end-exclusive), `h.slice(a, b)` and `h.chunks(chars)` return views, which print their text: print only
        what you must read, and hand views to `rlm.infer` / `rlm.map`."""
        given = [item for item in (source, path, text, data) if item is not None]
        if len(given) != 1:
            raise ValueError("rlm.load takes exactly one of a path, text, or bytes")
        file_path: Path | None = None
        if path is not None or isinstance(source, Path):
            file_path = Path(path if path is not None else source)
        elif isinstance(source, str) and "\n" not in source and len(source) < _MAX_PATH_TEXT and os.path.isfile(source):
            file_path = Path(source)
        if file_path is not None:
            raw = file_path.read_bytes()
            content = raw.decode("utf-8", errors="replace")
            label = label or file_path.name
        elif data is not None or isinstance(source, (bytes, bytearray)):
            content = bytes(data if data is not None else source).decode("utf-8", errors="replace")
            label = label or "bytes"
        else:
            content = text if text is not None else source
            if not isinstance(content, str):
                raise TypeError("rlm.load takes a path, str, or bytes")
            label = label or "text"
        encoded = content.encode("utf-8", errors="surrogatepass")
        digest = "sha256:" + hashlib.sha256(encoded).hexdigest()
        reply = await self._bridge.request("rlm.load", {
            "digest": digest,
            "label": label,
            "size": len(encoded),
            "chars": len(content),
            "source": str(file_path) if file_path is not None else None,
        })
        if not reply.get("stored"):
            target = Path(reply["path"])
            target.parent.mkdir(parents=True, exist_ok=True)
            temporary = target.with_name(target.name + f".{os.getpid()}.tmp")
            temporary.write_bytes(encoded)
            os.replace(temporary, target)
        existing = self._handles.get(digest)
        if existing is not None and existing.label == label:
            return existing
        handle = ContextHandle(existing._text if existing is not None else content, digest, label, len(encoded))
        self._handles[digest] = handle
        return handle

    async def open(self, digest: str, label: str | None = None) -> ContextHandle:
        """Reopen a stored handle by digest (for example inside a deeper frame, or after a kernel restart)."""
        existing = self._handles.get(digest)
        if existing is not None:
            return existing
        reply = await self._bridge.request("rlm.load", {"digest": digest})
        encoded = Path(reply["path"]).read_bytes()
        if "sha256:" + hashlib.sha256(encoded).hexdigest() != digest:
            raise ValueError(f"stored context {digest[:19]}… does not match its digest")
        handle = ContextHandle(encoded.decode("utf-8", errors="surrogatepass"), digest,
                               label or reply.get("label") or "context", len(encoded))
        self._handles[digest] = handle
        return handle

    async def _by_reference(self, contexts: list[list[dict[str, Any]]]) -> None:
        """Past _INLINE_BYTES of inline text (strings, views; shared context counts once per frame), intern each
        text as a handle so the request carries digests, not text. Frames see the same labelled views."""
        import json

        texts = [item for context in contexts for item in context if item["kind"] == "text"]
        if sum(len(json.dumps(item["text"])) for item in texts) <= _INLINE_BYTES:
            return
        interned: set[int] = set()
        for item in texts:
            if id(item) in interned or len(item["text"]) < 256:
                continue
            interned.add(id(item))
            handle = await self.load(text=item["text"], label=item["label"])
            item.clear()
            item.update(handle._wire())

    async def infer(self, task: str, context: Any = None, *, contract: Any = None, budget: Any = None,
                    model: str | None = None, max_repairs: int | None = None, timeout_ms: int | None = None) -> Any:
        """Run one private inference frame over explicit context views. Returns the contract-validated value
        (or the reply text without a contract), an `Incomplete` when budget or repairs run out, and raises
        `InferenceError` when the frame fails. `max_repairs` defaults to 2 re-asks (1 for a scalar contract).

        The frame is a sub-model that sees only `task` and the `context` views or strings: no transcript, no
        tools. `contract` is a JSON schema, int/str/float/bool/list/dict, list[T], Literal[...], T | None, a
        dataclass or `{'field': type}`. Large text goes to the host by handle. An `Incomplete` is falsy
        (`.status`, `.spent`, `.last_outputs`), not an exception. `budget=Budget(calls, tokens, depth)` caps the
        frame subtree; the frame's responses also count once toward the root's own turn, token and cost limits."""
        wired = _wire_context(context)
        await self._by_reference([wired])
        reply = await self._bridge.request("rlm.infer", {
            "task": task,
            "context": wired,
            "contract": _schema(contract),
            "budget": _budget(budget),
            "model": model,
            "max_repairs": max_repairs,
            "timeout_ms": timeout_ms,
        })
        return _outcome(reply, raise_errors=True)

    async def map(self, tasks: Any, items: Any = None, *, context: Any = None, contract: Any = None,
                  budget: Any = None, model: str | None = None, max_repairs: int | None = None,
                  concurrency: int = 8, timeout_ms: int | None = None) -> "MapResults":
        """Fan frames out under one shared budget, preserving order.

        `rlm.map(task, items)` runs `task` once per item (a view, a string, or a list of them);
        `rlm.map([task, ...])` runs each task. `context` is shared by every frame (sent before each item, so
        the frames share a cacheable prefix). Entries are values, `Incomplete`, or `FrameError`.

        Without `budget=Budget(tokens=...)` a top-level map is limited to the host's default token budget
        (ULTRON_RLM_MAP_TOKENS, 500,000 by default); frames past it come back `Incomplete`. The result is a
        list with `.spent` ({calls, tokens}), `.budget` and `.remaining`, and one summary line is printed.
        Every frame is a model request: filter with code first and give each frame only what it must judge
        (a section, a few KB), not whole files; compute exact numbers in plain Python. `await rlm.frames()`
        lists traces.

        Example:
            h = await rlm.load('app.log')
            hits = h.search(r'ERROR .*timeout', limit=8)
            causes = await rlm.map('Root cause of this failure, 10 words max.',
                                   [h.lines(m['line'] - 20, m['line'] + 5) for m in hits], contract=str)"""
        shared = _wire_context(context)
        if isinstance(tasks, str):
            if items is None:
                raise ValueError("rlm.map(task, items): items is required with a single task")
            frames = [{"task": tasks, "context": shared + _wire_context(item)} for item in items]
        else:
            tasks = list(tasks)
            if items is not None:
                items = list(items)
                if len(items) != len(tasks):
                    raise ValueError("rlm.map(tasks, items): tasks and items must have the same length")
                frames = [{"task": task, "context": shared + _wire_context(item)} for task, item in zip(tasks, items)]
            else:
                frames = [{"task": task, "context": shared} for task in tasks]
        await self._by_reference([frame["context"] for frame in frames])
        reply = await self._bridge.request("rlm.map", {
            "frames": frames,
            "contract": _schema(contract),
            "budget": _budget(budget),
            "model": model,
            "max_repairs": max_repairs,
            "concurrency": concurrency,
            "timeout_ms": timeout_ms,
        })
        results = MapResults(_outcome(item, raise_errors=False) for item in reply["results"])
        budget = reply.get("budget") if isinstance(reply.get("budget"), dict) else {}
        results.spent = dict(budget.get("spent") or {})
        results.budget = dict(budget.get("limits") or {})
        results.remaining = dict(budget.get("remaining") or {})
        print(results.summary())
        return results

    async def frames(self, trace_id: str | None = None, limit: int = 20) -> Any:
        """Recent frame summaries, or one frame's full trace by id."""
        payload: dict[str, Any] = {"limit": limit}
        if trace_id is not None:
            payload = {"id": trace_id}
        return await self._bridge.request("rlm.frames", payload)


def install(namespace: dict[str, Any], bridge: Any) -> None:
    """Attach load/open/infer/map/frames to the kernel's `rlm` object and export the result types."""
    inference = Inference(bridge)
    rlm = namespace["rlm"]
    for name in ("load", "open", "infer", "map", "frames"):
        setattr(rlm, name, getattr(inference, name))
    namespace["Budget"] = Budget
    namespace["ContextHandle"] = ContextHandle
    namespace["ContextView"] = ContextView
    namespace["Incomplete"] = Incomplete
    namespace["MapResults"] = MapResults
    namespace["FrameError"] = FrameError
    namespace["InferenceError"] = InferenceError
