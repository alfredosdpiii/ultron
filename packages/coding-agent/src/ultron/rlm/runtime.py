from __future__ import annotations

import ast
import asyncio
import base64
import contextlib
import hashlib
import io
import json
import linecache
import math
import os
import re
import signal
import sys
import threading
import tokenize
import traceback
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from agents_api import Agents, Workflows
from memory_api import Memory, Refinements
from family_api import AgentMessages
from progress_api import Progress
from skills_api import Skills
from schedules_api import Goals, Schedules
from instances_api import Instances
from grants_api import Grants
from release_gate_api import ReleaseGates
import agent_class_api
from context_api import Context
from infer_api import install as install_inference
from hints_api import Hints
from secret_patterns import REDACTION_MARKER
from tools_api import Mcp, McpError, ToolCall, ToolError, ToolResult, Tools

@dataclass
class SpawnHandle:
    rlm_child_id: str
    name: str
    session_dir: str
    model: str
    timeout_ms: int
    parent_branch_anchor: str

    @property
    def id(self) -> str:
        """The child's id (``rlm_child_id``), as ``agents.result(id)`` and runtime events name it."""
        return self.rlm_child_id

    def __repr__(self) -> str:
        return f"SpawnHandle(name={self.name!r}, rlm_child_id={self.rlm_child_id!r})"


# json.dumps escapes non-ASCII, so a line's length is its byte count; kept under kernel.ts MAX_FRAME_BYTES.
_MAX_HOST_FRAME_BYTES = 1024 * 1024 - 1024


class HostBridge:
    def __init__(self) -> None:
        self._host_requests: dict[str, asyncio.Future[Any]] = {}
        self._counter = 0
        self._write_lock = asyncio.Lock()

    async def request(self, request_type: str, payload: dict[str, Any] | None = None) -> Any:
        self._counter += 1
        request_id = f"host-{self._counter}"
        line = json.dumps({
            "event": "host_request",
            "id": request_id,
            "type": request_type,
            "payload": payload or {},
        }, separators=(",", ":")) + "\n"
        # The host kills a kernel that writes a frame over 1 MiB, losing every variable: refuse it here instead.
        if len(line) > _MAX_HOST_FRAME_BYTES:
            raise ValueError(f"{request_type} request is {len(line):,} bytes, over the kernel's 1 MiB frame: pass "
                             "rlm.load handles or file paths instead of inline text, or send it in parts")
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._host_requests[request_id] = future
        async with self._write_lock:
            _write_frame(line)
        try:
            return await future
        finally:
            self._host_requests.pop(request_id, None)

    def resolve(self, request_id: str, payload: Any = None, error: str | None = None) -> None:
        future = self._host_requests.get(request_id)
        if future is None or future.done():
            return
        if error:
            future.set_exception(RuntimeError(error))
        else:
            future.set_result(payload)


class RLMNamespace:
    """Subagents and bounded inference.

    `h = await rlm.spawn(task, name=...)` starts a subagent with its own REPL (a full agent: it re-sends the
    system prompt and its own transcript on every turn, so use it for independent multi-step work, not for
    reading files); `await rlm.collect([h])` waits for results; a subagent ends with `rlm.finish(...)`;
    `rlm.list_subagents()`, `rlm.delete_subagent(id)`. `rlm.load`, `rlm.open`, `rlm.infer`, `rlm.map` and `rlm.frames` are the
    bounded-inference API (`help(rlm.map)`). `rlm.jobs()` / `rlm.job(id)` recover shell jobs.

    Search before delegating, over many files or a large input: search the concept and its synonyms in code,
    print one compact line per candidate, read the deciding passages of the unclear ones yourself, and use
    `rlm.map` only for what a line or two cannot settle. For example:

        import re
        from pathlib import Path
        docs = {p.stem: p.read_text() for p in Path("reports").glob("*.md")}
        topic = re.compile(r"certific|\bTLS\b|\bSSL\b|x\.?509", re.I)
        event = re.compile(r"expir|lapsed|notAfter|validity", re.I)
        hits = {k: [l.strip() for l in t.splitlines() if topic.search(l) and event.search(l)] for k, t in docs.items()}
        for k, lines in hits.items():
            if lines: print(k, " | ".join(l[:150] for l in lines[:3]))

    Then print the root-cause lines of the unclear candidates, decide, and check synonyms you may have missed.
    """

    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    async def spawn(self, prompt: str, **kwargs: Any) -> SpawnHandle:
        """Start a subagent and return its SpawnHandle at once (`h.rlm_child_id`); options: name (required),
        model, thinking, timeout_ms, depth.

        The child has its own REPL and your tools and shares your filesystem, but not your conversation: give
        a self-contained brief (goal, paths, constraints, what to return). Its final reply is its result.
        Start several at once, then do only separate work of your own: never check on children through their
        files, logs or progress, since their results come to you. With nothing of your own left, wait for free
        with `await rlm.collect(handles)`, or (completion events on) end your turn: each end arrives as a
        `child_done` event.
        `await rlm.collect([h])` returns `[{"id": ..., "result": {"status": "succeeded", "value": <answer>,
        "verdict": {...} | None, "check": {"outcome": ...}}}]` (check each status): trust a verdict only when
        `check["outcome"] == "verified"`; `unobserved` lists files it claimed but did not change, `unreported`
        files that changed while it ran without being declared (maybe by concurrent work). A subagent does its brief itself unless you pass `depth=N` (1 lets it spawn its
        own subagents, 2 lets those spawn too; at most 3 levels in all), and every response they make counts
        toward the root's turn, token and cost limits. Spawn for independent multi-step work, never to read or
        classify documents (narrow with code and `rlm.map` instead)."""
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("rlm.spawn prompt must be a non-empty string")
        name = kwargs.get("name")
        if not isinstance(name, str) or not name.strip():
            raise ValueError("rlm.spawn name must be a non-empty string")
        allowed = {"name", "model", "thinking", "timeout_ms", "depth"}
        unknown = sorted(set(kwargs) - allowed)
        if unknown:
            raise TypeError(f"rlm.spawn unknown options: {', '.join(unknown)}")
        timeout_ms = kwargs.get("timeout_ms")
        if timeout_ms is not None and (isinstance(timeout_ms, bool) or not isinstance(timeout_ms, (int, float)) or timeout_ms < 1):
            raise ValueError("rlm.spawn timeout_ms must be a positive number")
        depth = kwargs.get("depth")
        if depth is not None and (isinstance(depth, bool) or not isinstance(depth, int) or depth < 0):
            raise ValueError("rlm.spawn depth must be a non-negative integer")
        result = await self._bridge.request("rlm.spawn", {
            "prompt": prompt,
            "kwargs": kwargs,
        })
        return SpawnHandle(**result)

    async def finish(self, status: str, summary: str, *, evidence: Any = (), outputs: dict[str, Any] | None = None,
                     changed_files: Any = ()) -> dict[str, Any]:
        """As a subagent, record your verdict, then end your turn with a short reply.

        status: "passed" (the brief is done), "failed" (it could not be done) or "blocked" (something outside
        you stopped it). summary: what was done or what blocked it. evidence: concrete strings, each a command
        with its outcome ("pytest -q: exit 0, 12 passed") or a file with lines ("src/a.py:40-52 handles X");
        "passed" is rejected without it. outputs: named results for the parent. changed_files: every file
        you (or your subagents) created, edited or deleted, relative to the working directory.

        The host checks changed_files against the files that changed on disk while you ran and shows the
        parent any mismatch. A rejected call raises with the reasons: fix them and call again (a few attempts)."""
        payload: dict[str, Any] = {
            "status": status,
            "summary": summary,
            "evidence": list(evidence) if isinstance(evidence, (list, tuple)) else evidence,
            "outputs": {} if outputs is None else outputs,
            "changed_files": [str(p) for p in changed_files] if isinstance(changed_files, (list, tuple, set)) else changed_files,
        }
        return await self._bridge.request("rlm.finish", payload)

    async def list_subagents(self) -> list[dict[str, Any]]:
        result = await self._bridge.request("rlm.list_subagents")
        return result.get("subagents", []) if isinstance(result, dict) else []

    async def collect(self, selectors: list[str] | None = None, timeout_ms: int = 0) -> list[dict[str, Any]]:
        """Wait for subagents (SpawnHandles or ids; all of yours when empty) and return their results:
        `[{"id": ..., "result": {"status": ..., "value" | "error": ..., "verdict": ..., "check": ...}}]`
        (`help(rlm.spawn)`)."""
        if isinstance(selectors, (str, SpawnHandle)):
            selectors = [selectors]
        # A SpawnHandle from rlm.spawn selects its child by id.
        selectors = [s.rlm_child_id if isinstance(s, SpawnHandle) else s for s in (selectors or [])]
        result = await self._bridge.request("rlm.collect", {
            "selectors": selectors,
            "timeout_ms": timeout_ms,
        })
        return result.get("results", []) if isinstance(result, dict) else []

    async def delete_subagent(self, selector: str) -> dict[str, Any]:
        result = await self._bridge.request("rlm.delete_subagent", {"selector": selector})
        return result if isinstance(result, dict) else {"result": result}

    async def find_models(self, query: str = "", limit: int = 8) -> list[dict[str, Any]]:
        result = await self._bridge.request("rlm.find_models", {"query": query, "limit": limit})
        return result.get("models", []) if isinstance(result, dict) else []

    async def host_request(self, request_type: str, payload: dict[str, Any] | None = None) -> Any:
        return await self._bridge.request(request_type, payload)

    async def jobs(self) -> list["ShellJob"]:
        """Shell jobs started with bash(..., yield_after=...), newest first (without their text)."""
        result = await self._bridge.request("shell.list", {})
        return [ShellJob(self._bridge, item) for item in (result if isinstance(result, list) else [])]

    async def job(self, job_id: str) -> "ShellJob":
        """Recover a shell job's handle by id (after the variable was lost or the kernel restarted)."""
        if not isinstance(job_id, str) or not job_id:
            raise ValueError("job id must be a non-empty string")
        return ShellJob(self._bridge, await self._bridge.request("shell.get", {"id": job_id}))

class BackgroundNamespace:
    """Long-running background agent jobs that outlive the turn.

    `await background.start(prompt)` starts one; `background.list()`, `background.inspect(id)`,
    `background.result(id)` and `background.stop(id)` manage them. Completion arrives as a `task_done` event.
    """

    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    async def start(self, prompt: str, *, model: str | None = None, key: str | None = None, timeout_ms: int | None = None) -> dict[str, Any]:
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("background.start prompt must be non-empty")
        payload: dict[str, Any] = {"prompt": prompt}
        if model is not None: payload["model"] = model
        if key is not None: payload["key"] = key
        if timeout_ms is not None: payload["timeout_ms"] = timeout_ms
        result = await self._bridge.request("background.start", payload)
        return result if isinstance(result, dict) else {"result": result}

    async def list(self) -> list[dict[str, Any]]:
        result = await self._bridge.request("background.list", {})
        return result if isinstance(result, list) else []

    async def inspect(self, job_id: str) -> dict[str, Any]:
        return await self._bridge.request("background.inspect", {"id": job_id})

    async def stop(self, job_id: str) -> dict[str, Any]:
        return await self._bridge.request("background.stop", {"id": job_id})

    async def result(self, job_id: str) -> Any:
        return await self._bridge.request("background.result", {"id": job_id})


class JevNamespace:
    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    async def triage(self, prompt: str) -> dict[str, Any]:
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("jev.triage prompt must be non-empty")
        result = await self._bridge.request("jev.triage", {"prompt": prompt})
        return result if isinstance(result, dict) else {"result": result}

    async def recall(self, prompt: str) -> dict[str, Any]:
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("jev.recall prompt must be non-empty")
        result = await self._bridge.request("jev.recall", {"prompt": prompt})
        return result if isinstance(result, dict) else {"result": result}

# A command's full output is read back from the host's spill file up to this size.
_BASH_MAX_OUTPUT_BYTES = 64 * 1024 * 1024


class BashOutput(str):
    """The combined stdout/stderr of a shell command, as a string (nano-rlm's bash skill).

    A nonzero exit, a timeout or a cancellation is appended as a bracketed status line, so
    printing the string always shows whether the command failed. The details are attributes:
    .output (raw output), .exit_code, .ok, .timed_out, .cancelled, .truncated (output cut
    short), .full_output_path. Earlier cells used a dict; ``out["exit_code"]`` still works.

    A command still running when ``bash`` stopped waiting (the default ``yield_after``) keeps
    running as a host job: then .running is True, .job is its ShellJob, .exit_code is None,
    .ok is False, the text is the output so far plus a note, and its end arrives as an event.
    """

    output: str
    exit_code: int | None
    timed_out: bool
    cancelled: bool
    truncated: bool
    full_output_path: str | None
    running: bool
    job: "ShellJob | None"

    def __new__(cls, output: str, exit_code: int | None, *, timed_out: bool = False, cancelled: bool = False,
                truncated: bool = False, full_output_path: str | None = None, timeout: float | None = None,
                job: "ShellJob | None" = None, waited: float | None = None) -> "BashOutput":
        text = output.strip()
        running = job is not None and job.running
        if running:
            events = _async_events_enabled()
            waited_text = f" after {waited:g} s" if waited is not None else ""
            text += (
                f"\n[still running as job {job.id}{waited_text}; its completion will arrive as a runtime event, "
                "or `await <result>.job.result()` to wait]"
                if events
                else f"\n[still running as job {job.id}{waited_text}; `await <result>.job.result()` waits for it]"
            )
        elif timed_out:
            text += f"\n[timed out after {timeout:g}s]"
        elif cancelled:
            text += "\n[cancelled]"
        elif exit_code not in (0, None):
            text += f"\n[exit code {exit_code}]"
        if truncated:
            text += f"\n[output truncated; full output in {full_output_path}]" if full_output_path else "\n[output truncated]"
        self = super().__new__(cls, text.strip() or "(no output)")
        self.output = output
        self.exit_code = exit_code
        self.timed_out = timed_out
        self.cancelled = cancelled
        self.truncated = truncated
        self.full_output_path = full_output_path
        self.running = running
        self.job = job
        return self

    @property
    def ok(self) -> bool:
        return self.exit_code == 0 and not self.timed_out and not self.cancelled and not self.running

    @property
    def text(self) -> str:
        """The string itself (as ShellJob.text), for code written against a result object."""
        return str(self)

    def __getitem__(self, key: Any) -> Any:
        if isinstance(key, str):
            if key in ("output", "exit_code", "timed_out", "cancelled", "truncated", "full_output_path", "ok", "running", "job"):
                return getattr(self, key)
            raise KeyError(key)
        return str.__getitem__(self, key)

    def get(self, key: str, default: Any = None) -> Any:
        try:
            return self[key]
        except KeyError:
            return default


class ShellJob:
    """A shell command owned by the Ultron host (started with ``bash(cmd, yield_after=...)``).

    It keeps running after the cell ends, across kernel restarts and eviction; an Esc abort of the
    turn that started it, ``timeout=``, or ``await job.cancel()`` stops it. When it ends while you
    are not waiting on it, a ``<runtime_event kind="job_done">`` message arrives on its own: do not poll.

    Attributes: .id, .running, .status, .exit_code, .ok (clean exit 0), .text (combined output,
    at most 16 KiB: head and tail around a marker), .truncated, .timed_out, .cancelled,
    .output_path, .output_bytes, .elapsed_seconds, .error.
    """

    def __init__(self, bridge: "HostBridge", data: Any) -> None:
        self._bridge = bridge
        self._update(data)

    def _update(self, data: Any) -> None:
        if not isinstance(data, dict) or not isinstance(data.get("id"), str):
            raise RuntimeError("bash: the host returned no job")
        self.id: str = data["id"]
        self.command: str = str(data.get("command") or "")
        self.status: str = str(data.get("status") or "")
        self.running: bool = bool(data.get("running"))
        exit_code = data.get("exit_code")
        self.exit_code: int | None = exit_code if isinstance(exit_code, int) else None
        self.ok: bool = bool(data.get("ok"))
        text = data.get("text")
        self.text: str | None = text if isinstance(text, str) else None
        self.truncated: bool = bool(data.get("truncated"))
        self.timed_out: bool = bool(data.get("timed_out"))
        self.cancelled: bool = bool(data.get("cancelled"))
        self.output_path: str = str(data.get("output_path") or "")
        self.output_bytes: int = int(data.get("output_bytes") or 0)
        self.elapsed_seconds: float = float(data.get("elapsed_seconds") or 0)
        error = data.get("error")
        self.error: str | None = error if isinstance(error, str) else None

    @property
    def job(self) -> "ShellJob":
        """This handle, so ``out.job.result()`` works whether ``out`` is a BashOutput or a ShellJob."""
        return self

    async def result(self, wait: float | None = None) -> "ShellJob":
        """Wait for the job to end (at most ``wait`` seconds) and return this handle, refreshed."""
        if wait is not None and (isinstance(wait, bool) or not isinstance(wait, (int, float)) or wait < 0):
            raise ValueError("wait must be a non-negative number of seconds")
        payload: dict[str, Any] = {"id": self.id}
        if wait is not None:
            payload["wait"] = wait
        self._update(await self._bridge.request("shell.result", payload))
        return self

    async def cancel(self) -> "ShellJob":
        """Stop the job (its whole process tree) and return this handle, refreshed."""
        self._update(await self._bridge.request("shell.cancel", {"id": self.id}))
        return self

    async def read(self, cursor: int = 0, max_bytes: int = 65536) -> dict[str, Any]:
        """Read retained output from byte ``cursor``: {text, next_cursor, done, truncated}."""
        return await self._bridge.request("shell.read", {"id": self.id, "cursor": cursor, "max_bytes": max_bytes})

    def _status_line(self) -> str:
        if self.running:
            return f"[job {self.id} running {self.elapsed_seconds:g}s: completion arrives as a <runtime_event>; await job.result() to wait]"
        if self.status == "completed":
            return f"[job {self.id} exit code {self.exit_code} after {self.elapsed_seconds:g}s]"
        detail = f": {self.error}" if self.error else ""
        return f"[job {self.id} {self.status}{detail}]"

    def __str__(self) -> str:
        body = (self.text or "").strip()
        return f"{body}\n{self._status_line()}" if body else self._status_line()

    def __repr__(self) -> str:
        if self.text is None:
            return f"ShellJob(id={self.id!r}, status={self.status!r}, exit_code={self.exit_code!r}, command={self.command[:60]!r})"
        return str(self)


_JOB_MAX_WAIT_SECONDS = 3600
# How long a plain `await bash(cmd)` waits before a slow command continues as a host job (ULTRON_BASH_YIELD_AFTER).
_DEFAULT_BASH_YIELD_AFTER = 30.0


class _Default:
    """Marker for an argument left out (distinct from an explicit None)."""

    def __repr__(self) -> str:
        return "default"


_DEFAULT = _Default()


def _async_events_enabled() -> bool:
    return os.environ.get("ULTRON_ASYNC_EVENTS", "").strip().lower() not in ("off", "0", "false", "no")


def _default_yield_after() -> float | None:
    """ULTRON_BASH_YIELD_AFTER seconds (default 30); 0 or "off" makes plain bash block until the command ends."""
    raw = os.environ.get("ULTRON_BASH_YIELD_AFTER", "").strip().lower()
    if raw == "":
        return _DEFAULT_BASH_YIELD_AFTER
    if raw in ("off", "none", "false", "no"):
        return None
    try:
        value = float(raw)
    except ValueError:
        return _DEFAULT_BASH_YIELD_AFTER
    if not math.isfinite(value) or value < 0:
        return _DEFAULT_BASH_YIELD_AFTER
    return None if value == 0 else min(value, _JOB_MAX_WAIT_SECONDS)


# Outside a work tree `git diff` prints its whole option list (about 130 lines, 7 KB) after the one line that
# matters; every later turn re-reads it. Keep the warning and drop the list.
_GIT_DIFF_USAGE = re.compile(
    r"^usage: git diff --no-index [^\n]*\n(?:(?:[ \t][^\n]*|Diff [^\n]*|Other diff options[^\n]*|)(?:\n|$))*",
    re.MULTILINE,
)


def _quiet_git_usage(output: str) -> str:
    if "usage: git diff --no-index" not in output:
        return output
    return _GIT_DIFF_USAGE.sub("(not a git work tree: no diff; compare against the text you read instead)\n", output)


async def bash(command: str, timeout: float | None = None, yield_after: Any = _DEFAULT) -> Any:
    """Run a shell command in the working directory.

    Args:
        command: The command, run by the user's shell (bash). A string literal written in the call
            reaches bash as typed: its backslashes are not Python escapes (as if written r'''...'''),
            so a heredoc's program keeps its own '\\n'.
        timeout: Seconds before the command is killed (default: no limit).
        yield_after: Left out: wait up to ULTRON_BASH_YIELD_AFTER seconds (default 30) and return
            the output as a string; a command still running then keeps running as a host job and
            the string says so (.running True, .job its ShellJob; its completion arrives as a
            <runtime_event>). None: wait for the command however long it takes. A number of
            seconds: start the command as a host job and return a ShellJob after at most that
            long, finished (.running False) or still running; 0 returns at once.

    Returns:
        Without a yield_after number, a BashOutput: stdout and stderr combined, with "[exit code N]"
        appended when the command failed; .exit_code and .ok carry the status. With a number, a
        ShellJob (see help(ShellJob)).
    """
    if not isinstance(command, str) or not command.strip():
        raise ValueError("bash command must be a non-empty string")
    if timeout is not None and (isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or timeout <= 0):
        raise ValueError("bash timeout must be a positive number of seconds")
    auto: float | None = None
    if yield_after is _DEFAULT:
        auto = _default_yield_after()
    elif yield_after is not None:
        if isinstance(yield_after, bool) or not isinstance(yield_after, (int, float)) or yield_after < 0:
            raise ValueError("bash yield_after must be a non-negative number of seconds")
        job_payload: dict[str, Any] = {"command": command, "yield_after": min(float(yield_after), _JOB_MAX_WAIT_SECONDS)}
        if timeout is not None:
            job_payload["timeout"] = timeout
        return ShellJob(_STATE.bridge, await _STATE.bridge.request("shell.run", job_payload))
    payload: dict[str, Any] = {"command": command}
    if timeout is not None:
        payload["timeout"] = timeout
    if auto is not None:
        payload["yield_after"] = auto
    result = await _STATE.bridge.request("bash", payload)
    if not isinstance(result, dict):
        raise RuntimeError("bash: the host returned no result")
    output = str(result.get("output") or "")
    truncated = bool(result.get("truncated"))
    path = result.get("full_output_path")
    job_data = result.get("job")
    job = ShellJob(_STATE.bridge, job_data) if result.get("running") and isinstance(job_data, dict) else None
    # A job's spill file may hold only the head of a very long output (partial_file): the text is better then.
    if truncated and isinstance(path, str) and path and job is None and not result.get("partial_file"):
        # The host keeps only the tail; the whole output is in its spill file.
        with contextlib.suppress(OSError):
            with open(path, "rb") as handle:
                data = handle.read(_BASH_MAX_OUTPUT_BYTES + 1)
            if len(data) <= _BASH_MAX_OUTPUT_BYTES:
                output = data.decode("utf-8", errors="replace")
                truncated = False
    exit_code = result.get("exit_code")
    return BashOutput(
        _quiet_git_usage(output),
        exit_code if isinstance(exit_code, int) else None,
        timed_out=bool(result.get("timed_out")),
        cancelled=bool(result.get("cancelled")),
        truncated=truncated,
        full_output_path=path if isinstance(path, str) else None,
        timeout=timeout,
        job=job,
        waited=auto,
    )


# `read` returns a file's text up to this size and a ContextHandle above it (ULTRON_READ_HANDLE_BYTES).
_DEFAULT_READ_HANDLE_BYTES = 256 * 1024


def _read_handle_bytes() -> int:
    try:
        value = int(os.environ.get("ULTRON_READ_HANDLE_BYTES", "").strip())
    except ValueError:
        return _DEFAULT_READ_HANDLE_BYTES
    return value if value > 0 else _DEFAULT_READ_HANDLE_BYTES


async def read(path: str | os.PathLike[str]) -> Any:
    """Read a text file (relative to the working directory, or absolute).

    Returns the text (a str) for a file up to ULTRON_READ_HANDLE_BYTES (default 256 KiB). A larger
    file is loaded with ``rlm.load`` and returned as a ContextHandle, with a one-line note naming its
    size and digest: program over it with ``h.search(regex)``, ``h.lines(a, b)``, ``h.chunks(n)``,
    ``h.count(regex)`` or ``rlm.map(task, h.chunks(n))`` instead of reading it whole.
    """
    if not isinstance(path, (str, os.PathLike)) or not str(path):
        raise ValueError("read path must be a non-empty string")
    filepath = Path(path)
    if not filepath.is_absolute():
        filepath = Path.cwd() / filepath
    size = filepath.stat().st_size
    with filepath.open("rb") as handle:
        kind = _image_kind(handle.read(16))
    if kind is not None:
        return (
            f"[read] {path} is a {kind.upper()} image ({size:,} bytes); read() returns text only. "
            f"Use `await view_image({str(path)!r})` to look at it."
        )
    if size <= _read_handle_bytes():
        return filepath.read_bytes().decode("utf-8", errors="replace")
    handle = await _STATE.namespace["rlm"].load(path=filepath)
    print(
        f"[read] {path} is {size:,} bytes (over {_read_handle_bytes():,}), so it was loaded as a handle, not text: "
        f"{handle!r}. Use h.search(regex), h.lines(a, b), h.chunks(n) or rlm.map(task, h.chunks(n)) on it."
    )
    return handle


def _image_kind(head: bytes) -> str | None:
    """The image format named by a file's magic bytes (png, jpeg, gif, webp), or None."""
    if head.startswith(b"\x89PNG\r\n\x1a\n"):
        return "png"
    if head.startswith(b"\xff\xd8\xff"):
        return "jpeg"
    if head.startswith((b"GIF87a", b"GIF89a")):
        return "gif"
    if len(head) >= 12 and head[:4] == b"RIFF" and head[8:12] == b"WEBP":
        return "webp"
    return None


def _figure_png(source: Any) -> bytes | None:
    """PNG bytes of a matplotlib figure (or pyplot, or an Axes), else None."""
    figure = source
    if not hasattr(figure, "savefig") and hasattr(figure, "get_figure"):
        figure = figure.get_figure()
    if not callable(getattr(figure, "savefig", None)):
        return None
    buffer = io.BytesIO()
    figure.savefig(buffer, format="png", bbox_inches="tight")
    return buffer.getvalue()


def _pil_png(source: Any) -> bytes | None:
    """PNG bytes of a PIL image when PIL is available, else None."""
    try:
        from PIL import Image as PILImage  # type: ignore[import-not-found]
    except Exception:
        return None
    if not isinstance(source, PILImage.Image):
        return None
    image = source
    if image.mode not in ("1", "L", "LA", "P", "RGB", "RGBA"):
        image = image.convert("RGBA")
    buffer = io.BytesIO()
    image.save(buffer, format="PNG")
    return buffer.getvalue()


async def view_image(path_or_bytes: Any, *, detail: str | None = None) -> str:
    """Show an image to the model: it is attached to this cell's tool result as an image.

    Accepts a path (png, jpeg, gif or webp, detected from the file's bytes, not its extension), raw image bytes,
    a PIL image, or a matplotlib figure (rendered to PNG). Oversized images are downscaled to the model's limits;
    ``detail="low"`` caps the long side at 512 px. At most 8 images per cell. Returns a short description
    (dimensions, format, bytes). The image itself never appears in printed output.
    """
    if detail is not None and detail not in ("low", "high", "auto"):
        raise ValueError("view_image detail must be None, 'low', 'high' or 'auto'")
    data: bytes | None = None
    if isinstance(path_or_bytes, (str, os.PathLike)):
        if not str(path_or_bytes):
            raise ValueError("view_image path must be a non-empty string")
        filepath = Path(path_or_bytes)
        if not filepath.is_absolute():
            filepath = Path.cwd() / filepath
        if not filepath.is_file():
            raise FileNotFoundError(f"{path_or_bytes} not found")
        with filepath.open("rb") as handle:
            if _image_kind(handle.read(16)) is None:
                raise ValueError(f"{path_or_bytes} is not a png, jpeg, gif or webp image")
    elif isinstance(path_or_bytes, (bytes, bytearray, memoryview)):
        data = bytes(path_or_bytes)
    else:
        data = _pil_png(path_or_bytes)
        if data is None:
            data = _figure_png(path_or_bytes)
        if data is None:
            raise TypeError(
                "view_image takes a path, image bytes, a PIL image or a matplotlib figure, "
                f"not {type(path_or_bytes).__name__}"
            )
    temporary: str | None = None
    if data is not None:
        kind = _image_kind(data[:16])
        if kind is None:
            raise ValueError("view_image bytes are not a png, jpeg, gif or webp image")
        import tempfile

        descriptor, temporary = tempfile.mkstemp(prefix="ultron-view-image-", suffix=f".{kind}")
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
        filepath = Path(temporary)
    payload: dict[str, Any] = {"path": str(filepath)}
    if detail is not None:
        payload["detail"] = detail
    try:
        reply = await _STATE.bridge.request("rlm.view_image", payload)
    finally:
        if temporary is not None:
            with contextlib.suppress(OSError):
                os.unlink(temporary)
    if not isinstance(reply, dict):
        return str(reply)
    description = str(reply.get("description", "image attached"))
    note = reply.get("note")
    return f"{description}\n{note}" if note else description


def _file_hooks_enabled() -> bool:
    return os.environ.get("ULTRON_FILE_HOOKS") == "1"


async def _check_write(filepath: Path, content: str, display: str) -> None:
    """Show a proposed write to the host's before-write hooks (Loki, extensions) before it happens.

    A hook that blocks raises ValueError with its diagnostic, and the caller writes nothing. Notes (advisory
    findings, or a hook that could not check in time, so the write proceeds unchecked) are printed.
    """
    if not _file_hooks_enabled():
        return
    try:
        reply = await _STATE.bridge.request(
            "files.before_write", {"writes": [{"path": str(filepath), "content": content}]}
        )
    except (RuntimeError, ValueError) as error:
        print(f"[write hooks] {display} was written unchecked: {error}")
        return
    results = reply.get("results") if isinstance(reply, dict) else None
    result = results[0] if isinstance(results, list) and results and isinstance(results[0], dict) else {}
    for note in result.get("notes") or []:
        print(note)
    if result.get("blocked"):
        raise ValueError(f"{display} was not written: {result.get('reason') or 'a write hook blocked it'}")


async def write(path: str | os.PathLike[str], text: str) -> str:
    """Create or overwrite a text file (parent directories are created).

    Args:
        path: File path, relative to the working directory or absolute.
        text: The complete new content.

    Returns:
        A confirmation message. The write is checked first (Loki guardrails, extension hooks): a check that
        blocks raises ValueError with its finding and nothing is written. Several writes in one cell are
        checked in parallel with ``await asyncio.gather(write(a, x), edit(b, ...))``.
    """
    if not isinstance(path, (str, os.PathLike)) or not str(path):
        raise ValueError("write path must be a non-empty string")
    if not isinstance(text, str):
        raise TypeError(f"write text must be a str, not {type(text).__name__}")
    filepath = Path(path)
    if not filepath.is_absolute():
        filepath = Path.cwd() / filepath
    await _check_write(filepath, text, str(path))
    filepath.parent.mkdir(parents=True, exist_ok=True)
    filepath.write_text(text)
    return f"Wrote {path} ({len(text.encode('utf-8')):,} bytes)"


async def edit(path: str, old_str: str, new_str: str) -> str:
    """Replace a unique string in a file (nano-rlm's edit skill).

    Args:
        path: File path, relative to the working directory or absolute.
        old_str: Exact string to find; it must appear exactly once in the file.
        new_str: Replacement string.

    Returns:
        A confirmation message. Raises FileNotFoundError when the file is missing and
        ValueError when old_str is absent or appears more than once, or when a write check
        (Loki guardrails, extension hooks) blocks the new content; then nothing is written.
    """
    filepath = Path(path)
    if not filepath.is_absolute():
        filepath = Path.cwd() / filepath
    if not filepath.exists():
        raise FileNotFoundError(f"{path} not found")
    content = filepath.read_text()
    count = content.count(old_str)
    if count == 0 and isinstance(old_str, str):
        marker = REDACTION_MARKER.search(old_str)
        if marker is not None:
            # Cell output masks secrets, so text copied from it can hold a marker the file does not. Nothing is written.
            raise ValueError(
                f"old_str contains {marker.group(0)}, a mask that cell output shows in place of a secret; {path} "
                "holds the real value, unchanged. Make old_str the text before or after the secret (not the marker), "
                "or edit in code: `text = await read(path)` has the real content."
            )
    if count != 1:
        raise ValueError(f"old_str must appear exactly once in {path} (found {count})")
    updated = content.replace(old_str, new_str, 1)
    await _check_write(filepath, updated, str(path))
    filepath.write_text(updated)
    return f"Edited {path}"


def _env_limit(name: str) -> int:
    try:
        return max(0, int(os.environ.get(name, "0")))
    except ValueError:
        return 0


# Resource limits come from the host (kernel.ts), which resolves defaults and overrides. 0 disables one.
# Memory is RLIMIT_DATA per process, inherited by every subprocess a cell spawns. CPU is a per-cell budget:
# the soft RLIMIT_CPU is re-armed at each cell start, and the host kills the kernel if a cell overruns it.
_MAX_MEMORY_MB = _env_limit("ULTRON_RLM_MAX_MEMORY_MB")
_MAX_CPU_SECONDS = _env_limit("ULTRON_RLM_MAX_CPU_SECONDS")
# Exit codes the host maps to "exceeded its memory/CPU limit" when the runtime cannot report in-band.
_EXIT_MEMORY = 86
_EXIT_CPU = 87
_EXIT_INTERNAL = 70


class RlmCpuLimitExceeded(BaseException):
    """Raised in the cell when it uses more than ULTRON_RLM_MAX_CPU_SECONDS of CPU time."""


def _cpu_used() -> float:
    import resource

    usage = resource.getrusage(resource.RUSAGE_SELF)
    return usage.ru_utime + usage.ru_stime


def _arm_cpu_limit() -> None:
    """Give the process `_MAX_CPU_SECONDS` more CPU before SIGXCPU; children inherit the soft limit."""
    if _MAX_CPU_SECONDS <= 0:
        return
    try:
        import resource

        _soft, hard = resource.getrlimit(resource.RLIMIT_CPU)
        soft = math.ceil(_cpu_used()) + _MAX_CPU_SECONDS
        if hard != resource.RLIM_INFINITY:
            soft = min(soft, hard)
        resource.setrlimit(resource.RLIMIT_CPU, (soft, hard))
    except (ImportError, OSError, ValueError):
        pass


def _on_cpu_limit(_signum: int, _frame: Any) -> None:
    # Re-arm first so the error path gets a fresh budget instead of a signal every second.
    _arm_cpu_limit()
    if _STATE.cell_active:
        raise RlmCpuLimitExceeded(
            f"RLM cell exceeded its CPU limit of {_MAX_CPU_SECONDS} CPU-seconds (ULTRON_RLM_MAX_CPU_SECONDS)"
        )


def _apply_resource_limits() -> None:
    try:
        import resource
    except ImportError:
        return
    # A limit kill (SIGXCPU, or an out-of-memory abort) would otherwise write a core dump for the kernel
    # or any subprocess a cell starts, and desktop crash reporters announce every one. Limit hits are
    # expected, reported outcomes, so no process in the kernel's tree dumps core.
    if hasattr(resource, "RLIMIT_CORE"):
        try:
            resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        except (ValueError, OSError):
            pass
    if _MAX_MEMORY_MB > 0 and hasattr(resource, "RLIMIT_DATA"):
        limit = _MAX_MEMORY_MB * 1024 * 1024
        _soft, hard = resource.getrlimit(resource.RLIMIT_DATA)
        if hard != resource.RLIM_INFINITY:
            limit = min(limit, hard)
        # The hard limit is lowered too, so cell code cannot raise it again for itself or its children.
        resource.setrlimit(resource.RLIMIT_DATA, (limit, limit))
    if _MAX_CPU_SECONDS > 0 and hasattr(signal, "SIGXCPU"):
        signal.signal(signal.SIGXCPU, _on_cpu_limit)
        _arm_cpu_limit()


def _become_subreaper() -> None:
    # A cell may start a process that double-forks (or setsid()s and forks) so its parent exits. Linux would
    # reparent such an orphan to init or a subreaper outside the kernel, where the host's tree memory watchdog
    # neither counts nor kills it. As a child subreaper the kernel adopts every orphaned descendant instead.
    if not sys.platform.startswith("linux"):
        return
    try:
        import ctypes

        libc = ctypes.CDLL(None, use_errno=True)
        libc.prctl(36, 1, 0, 0, 0)  # PR_SET_CHILD_SUBREAPER
    except (OSError, AttributeError):
        pass


def _memory_limit_note() -> str:
    return (
        f"RLM kernel exceeded its memory limit ({_MAX_MEMORY_MB} MiB per process, ULTRON_RLM_MAX_MEMORY_MB); "
        "the allocation failed and the kernel keeps running"
    )


class RuntimeState:
    def __init__(self) -> None:
        self.bridge = HostBridge()
        self.cell_active = False
        self.execution_lock = asyncio.Lock()
        self.snapshot_path: Path | None = None
        # asyncio is pre-imported: the guide batches independent work with asyncio.gather.
        self.namespace: dict[str, Any] = {"__name__": "__main__", "asyncio": asyncio}
        # The stdlib names cells most often use without importing (a NameError costs a whole turn).
        self.namespace.update({"re": re, "json": json, "os": os, "Path": Path})
        self.namespace["rlm"] = RLMNamespace(self.bridge)
        self.namespace["agent_message"] = AgentMessages(self.bridge)
        self.namespace["jev"] = JevNamespace(self.bridge)
        self.namespace["background"] = BackgroundNamespace(self.bridge)
        self.namespace["bash"] = bash
        self.namespace["edit"] = edit
        self.namespace["write"] = write
        self.namespace["read"] = read
        self.namespace["view_image"] = view_image
        self.namespace["hints"] = Hints(self.bridge)
        # Extension tools (the pi-mcp-adapter's `mcp` gateway among them) as async skills.
        self.namespace["tools"] = Tools(self.bridge)
        self.namespace["mcp"] = Mcp(self.namespace["tools"])
        self.namespace["ToolCall"] = ToolCall
        self.namespace["ToolResult"] = ToolResult
        self.namespace["ToolError"] = ToolError
        self.namespace["McpError"] = McpError
        self.namespace["SpawnHandle"] = SpawnHandle
        self.namespace["ShellJob"] = ShellJob
        self.namespace["agents"] = Agents(self.bridge)
        self.namespace["workflows"] = Workflows(self.bridge)
        self.namespace["memory"] = Memory(self.bridge)
        self.namespace["refinements"] = Refinements(self.bridge)
        self.namespace["progress"] = Progress(self.bridge)
        self.namespace["skills"] = Skills(self.bridge)
        self.namespace["schedules"] = Schedules(self.bridge)
        self.namespace["goals"] = Goals(self.bridge)
        self.namespace["instances"] = Instances(self.bridge)
        self.namespace["grants"] = Grants(self.bridge)
        self.namespace["gates"] = ReleaseGates(self.bridge)
        self.namespace["ctx"] = Context(self.bridge)
        self.namespace["preview"] = preview
        agent_class_api.configure(self.bridge, lambda: self.namespace, preview)
        self.namespace["agent"] = agent_class_api.agent
        self.namespace["Agent"] = agent_class_api.Agent
        self.namespace["AgentCallError"] = agent_class_api.AgentCallError
        install_inference(self.namespace, self.bridge)
        # Declared instance state survives reset_scratch; every other name is invocation scratch.
        self.namespace["state"] = {}
        self.bindings = {name: value for name, value in self.namespace.items() if name != "state"}

    def reset_scratch(self) -> None:
        state = self.namespace.get("state", {})
        self.namespace.clear()
        self.namespace.update(self.bindings)
        self.namespace["state"] = state
        # Agent classes and their instances live in `state`; define the classes again for the fresh scratch.
        agent_class_api.rehydrate()

# Fields that identify a frame; never shortened to fit it.
_FRAME_KEYS = frozenset({"event", "id", "status"})


def _shrink_frame_value(value: Any, keep: int) -> Any:
    """Keep the head and tail of every string (in characters) and list (in items) within `keep`."""
    if isinstance(value, str):
        if len(value) <= keep:
            return value
        half = keep // 2
        cut = len(value) - 2 * half
        return f"{value[:half]}\n[... {cut} characters cut to fit the 1 MiB protocol frame ...]\n{value[len(value) - half:]}"
    if isinstance(value, (list, tuple)):
        items = list(value)
        if len(items) > keep:
            half = keep // 2
            items = [*items[:half], f"[... {len(items) - 2 * half} items cut to fit the 1 MiB protocol frame ...]",
                     *items[len(items) - half:]]
        return [_shrink_frame_value(item, keep) for item in items]
    if isinstance(value, dict):
        return {key: _shrink_frame_value(item, keep) for key, item in value.items()}
    return value


def _encode_frame(payload: dict[str, Any]) -> str:
    """One protocol line within the host's 1 MiB frame. The host ends a kernel that sends a larger frame, losing
    every variable, so an oversized output (a large capture budget, many names) is cut here, in the cell."""
    line = json.dumps(payload, default=str, separators=(",", ":"))
    keep = _MAX_HOST_FRAME_BYTES // 2
    while len(line) > _MAX_HOST_FRAME_BYTES and keep > 0:
        # JSON escaping can grow a character up to 12 bytes; halve until the frame fits.
        keep //= 2
        shrunk = {key: value if key in _FRAME_KEYS else _shrink_frame_value(value, keep) for key, value in payload.items()}
        line = json.dumps(shrunk, default=str, separators=(",", ":"))
    if len(line) > _MAX_HOST_FRAME_BYTES:
        # Only values json.dumps renders with str() can still be this large: keep the frame's identity.
        line = json.dumps({key: payload[key] for key in payload if key in _FRAME_KEYS}, separators=(",", ":"))
    return line + "\n"


# Protocol channel. With --protocol-fds (every platform but Windows) frames go to the private fd 3 and host
# messages arrive on fd 4, both close-on-exec, so fd 0/1/2 are plain streams: stdin is /dev/null, and bytes that
# anything writes to fd 1 or fd 2 (os.write, subprocesses, C code) are output the host attributes to the running
# cell rather than protocol. Without the flag (Windows) the protocol stays on stdout and stdin.
_PROTOCOL_FDS = (3, 4)
_PRIVATE_PROTOCOL = False
_PROTOCOL_OUT: Any = None
_PROTOCOL_IN: Any = None
# (st_dev, st_ino) of the pipes fd 1 and fd 2 were at startup; a flush marker goes only to the host's own pipe.
_RAW_STREAMS: dict[str, tuple[int, tuple[int, int]]] = {}
_LIBC_FFLUSH: Any = None


def _open_protocol() -> None:
    global _PRIVATE_PROTOCOL, _PROTOCOL_OUT, _PROTOCOL_IN, _LIBC_FFLUSH
    if "--protocol-fds" not in sys.argv[1:]:
        _PROTOCOL_OUT, _PROTOCOL_IN = sys.__stdout__.buffer, sys.__stdin__.buffer
        return
    out_fd, in_fd = _PROTOCOL_FDS
    # Close-on-exec: no process a cell starts (os.system, subprocess with close_fds=False) can reach the protocol.
    os.set_inheritable(out_fd, False)
    os.set_inheritable(in_fd, False)
    _PROTOCOL_OUT = os.fdopen(out_fd, "wb")
    _PROTOCOL_IN = os.fdopen(in_fd, "rb")
    _PRIVATE_PROTOCOL = True
    for name, fd in (("stdout", 1), ("stderr", 2)):
        try:
            stat = os.fstat(fd)
            _RAW_STREAMS[name] = (fd, (stat.st_dev, stat.st_ino))
        except OSError:
            pass
    try:
        import ctypes
        _LIBC_FFLUSH = ctypes.CDLL(None).fflush
    except Exception:
        _LIBC_FFLUSH = None
    import builtins

    def _no_input(prompt: Any = "") -> str:
        raise EOFError("input() is not available: the RLM kernel has no interactive stdin (it reads /dev/null); "
                       "put the data in the code, a variable or a file instead")

    builtins.input = _no_input


def _write_frame(line: str) -> None:
    _PROTOCOL_OUT.write(line.encode("utf-8"))
    _PROTOCOL_OUT.flush()


def emit(event: str, **fields: Any) -> None:
    _write_frame(_encode_frame({"event": event, **fields}))


def _flush_raw_output(request_id: Any) -> dict[str, bool] | None:
    """Write the cell's flush marker to fd 1 and fd 2, after anything buffered in Python or C stdio. The host
    finalizes the cell once it has read each marker written here, so raw output from the cell precedes it."""
    if not _PRIVATE_PROTOCOL:
        return None
    for stream in (sys.__stdout__, sys.__stderr__):
        try:
            stream.flush()
        except BaseException:
            pass
    if _LIBC_FFLUSH is not None:
        try:
            _LIBC_FFLUSH(None)
        except BaseException:
            pass
    marker = f"\x1eultron-rlm-flush:{request_id}\x1e".encode("utf-8")
    flushed: dict[str, bool] = {}
    for name in ("stdout", "stderr"):
        flushed[name] = False
        if name not in _RAW_STREAMS:
            continue
        fd, identity = _RAW_STREAMS[name]
        try:
            # Cell code may have closed or redirected the fd: the marker would never reach the host.
            stat = os.fstat(fd)
            if (stat.st_dev, stat.st_ino) != identity:
                continue
            view = memoryview(marker)
            while view:
                view = view[os.write(fd, view):]
            flushed[name] = True
        except BaseException:
            pass
    return flushed


_SNAPSHOT_FORMAT = "ultron-rlm-snapshot"
_SNAPSHOT_VERSION = 2


class _NonRestorable(Exception):
    pass


class SnapshotIntegrityError(Exception):
    pass


def _type_name(kind: type) -> str:
    return f"{kind.__module__}.{kind.__qualname__}"


def _encode_value(value: Any, mutable: set[int]) -> Any:
    """Encode only exact builtin data types. Every JSON object in the encoding is a tag,
    so tuples, sets, bytes and non-string dict keys round-trip as their own types."""
    kind = type(value)
    if value is None or kind in (bool, int, str, float):
        return value
    if kind is not list and isinstance(value, list) and getattr(kind, "_snapshot_as_list", False):
        # A list subclass that only adds metadata (rlm.map's MapResults) is kept as its plain items.
        kind = list
    if kind in (bytes, bytearray):
        if kind is bytearray:
            if id(value) in mutable:
                raise _NonRestorable("shared or cyclic mutable reference")
            mutable.add(id(value))
        return {"$" + kind.__name__: base64.b64encode(bytes(value)).decode("ascii")}
    if kind in (list, dict, set):
        if id(value) in mutable:
            raise _NonRestorable("shared or cyclic mutable reference")
        mutable.add(id(value))
    if kind is list:
        return [_encode_value(item, mutable) for item in value]
    if kind is tuple:
        return {"$tuple": [_encode_value(item, mutable) for item in value]}
    if kind in (set, frozenset):
        return {"$" + kind.__name__: [_encode_value(item, mutable) for item in value]}
    if kind is dict:
        return {"$dict": [[_encode_value(key, mutable), _encode_value(item, mutable)] for key, item in value.items()]}
    raise _NonRestorable(f"unsupported type {_type_name(kind)}")


def _decode_value(value: Any) -> Any:
    kind = type(value)
    if value is None or kind in (bool, int, str, float):
        return value
    if kind is list:
        return [_decode_value(item) for item in value]
    if kind is not dict or len(value) != 1:
        raise SnapshotIntegrityError("snapshot contains a malformed value")
    tag, body = next(iter(value.items()))
    if tag in ("$bytes", "$bytearray") and type(body) is str:
        raw = base64.b64decode(body.encode("ascii"), validate=True)
        return raw if tag == "$bytes" else bytearray(raw)
    if type(body) is not list:
        raise SnapshotIntegrityError("snapshot contains a malformed value")
    if tag == "$tuple":
        return tuple(_decode_value(item) for item in body)
    if tag == "$set":
        return {_decode_value(item) for item in body}
    if tag == "$frozenset":
        return frozenset(_decode_value(item) for item in body)
    if tag == "$dict":
        result = {}
        for pair in body:
            if type(pair) is not list or len(pair) != 2:
                raise SnapshotIntegrityError("snapshot contains a malformed dict")
            result[_decode_value(pair[0])] = _decode_value(pair[1])
        return result
    raise SnapshotIntegrityError(f"snapshot contains an unknown tag: {_text_preview(str(tag))}")


def _persistable_namespace() -> tuple[dict[str, Any], dict[str, str]]:
    """Encode restorable names and give every other user name an explicit reason."""
    encoded: dict[str, Any] = {}
    reasons: dict[str, str] = {}
    owners: dict[int, str] = {}
    for name, value in list(_STATE.namespace.items()):
        if name.startswith("__") or (name in _STATE.bindings and value is _STATE.bindings[name]):
            continue
        mutable: set[int] = set()
        try:
            candidate = _encode_value(value, mutable)
            json.dumps(candidate, allow_nan=True)
        except _NonRestorable as error:
            reasons[name] = str(error)
            continue
        except Exception as error:
            reasons[name] = f"not encodable: {type(error).__name__}"
            continue
        # Restoring separately encoded names would silently split an alias into copies.
        shared = sorted({owners[item] for item in mutable if item in owners})
        if shared:
            reasons[name] = "aliases mutable data of " + ", ".join(shared)
            for other in shared:
                encoded.pop(other, None)
                reasons.setdefault(other, "aliases mutable data of " + name)
            continue
        for item in mutable:
            owners[item] = name
        encoded[name] = candidate
    return encoded, reasons


def save_snapshot(path: str) -> dict[str, Any]:
    """Write a constrained data-only snapshot: a sha256 header line, then a JSON body.

    The digest detects corruption. Authenticity is the host's job: kernel.ts signs the file with an
    HMAC key this process never sees and verifies it before any restore.
    """
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    agent_class_api.flush()
    encoded, reasons = _persistable_namespace()
    skipped = sorted(reasons)
    body = json.dumps(
        {"names": encoded, "skipped": skipped, "reasons": reasons}, separators=(",", ":"), allow_nan=True
    ).encode("utf-8")
    digest = hashlib.sha256(body).hexdigest()
    header = json.dumps(
        {"format": _SNAPSHOT_FORMAT, "version": _SNAPSHOT_VERSION, "sha256": digest, "bytes": len(body)},
        separators=(",", ":"),
    ).encode("utf-8")
    content = header + b"\n" + body
    temporary = target.with_name(f".{target.name}.{os.getpid()}.tmp")
    temporary.write_bytes(content)
    os.replace(temporary, target)
    _STATE.snapshot_path = target
    return {
        "saved": sorted(encoded),
        "skipped": skipped,
        "reasons": reasons,
        "serializer": "json",
        "sha256": digest,
        # The host signs exactly these bytes; it refuses to sign a file that changed after this write.
        "content_sha256": hashlib.sha256(content).hexdigest(),
    }


_SIGNATURE_FORMAT = "ultron-rlm-snapshot-signature"


def restore_snapshot(path: str, verified_sha256: Any = None) -> dict[str, Any]:
    """Restore a snapshot the host has already authenticated.

    The host checks the HMAC signature line and passes the sha256 of the signed content; the bytes read
    here must hash to that value, so a file swapped after the host's check is refused as well.
    """
    target = Path(path)
    if not target.exists():
        return {"restored": [], "missing": True, "skipped": [], "reasons": {}}
    raw = target.read_bytes()
    signature_line, separator, content = raw.partition(b"\n")
    try:
        signature = json.loads(signature_line) if separator else None
    except ValueError:
        signature = None
    if type(signature) is not dict or signature.get("format") != _SIGNATURE_FORMAT:
        raise SnapshotIntegrityError(
            "snapshot integrity check failed: missing host signature (unsigned snapshots are refused)"
        )
    if type(verified_sha256) is not str or hashlib.sha256(content).hexdigest() != verified_sha256:
        raise SnapshotIntegrityError("snapshot integrity check failed: content differs from the host-verified snapshot")
    header_line, separator, body = content.partition(b"\n")
    try:
        header = json.loads(header_line) if separator else None
    except ValueError:
        header = None
    if (
        type(header) is not dict
        or header.get("format") != _SNAPSHOT_FORMAT
        or header.get("version") != _SNAPSHOT_VERSION
        or type(header.get("sha256")) is not str
        or type(header.get("bytes")) is not int
    ):
        raise SnapshotIntegrityError("snapshot integrity check failed: missing or unsupported header")
    if header["bytes"] != len(body) or hashlib.sha256(body).hexdigest() != header["sha256"]:
        raise SnapshotIntegrityError("snapshot integrity check failed: digest mismatch")
    data = json.loads(body.decode("utf-8"))
    names = data.get("names") if type(data) is dict else None
    skipped = data.get("skipped") if type(data) is dict else None
    reasons = data.get("reasons") if type(data) is dict else None
    if (
        type(names) is not dict
        or type(skipped) is not list
        or type(reasons) is not dict
        or any(type(name) is not str for name in [*names, *skipped, *reasons.values()])
    ):
        raise SnapshotIntegrityError("snapshot integrity check failed: malformed body")
    # Decode everything before touching the namespace, so a bad snapshot changes nothing.
    decoded = {name: _decode_value(value) for name, value in names.items()}
    _STATE.namespace.update(decoded)
    _STATE.snapshot_path = target
    agent_class_api.rehydrate()
    return {"restored": sorted(decoded), "missing": False, "skipped": sorted(skipped), "reasons": reasons}


_UNCLOSED = re.compile(r"^'([(\[{])' was never closed$")
_CLOSERS = {"(": ")", "[": "]", "{": "}"}


def _parse_cell(source: str) -> tuple[ast.Module, str, str | None]:
    """Parse a cell; a cell whose only fault is brackets left open at its very end (typically
    ``print(await bash('''...''')`` short of one ``)``) is closed there, with a note for the output."""
    try:
        return ast.parse(source, filename="<rlm-cell>", mode="exec"), source, None
    except SyntaxError as parse_error:
        original = error = parse_error
    added = ""
    for _ in range(3):
        match = _UNCLOSED.match(str(error.msg or ""))
        if match is None:
            break
        added += _CLOSERS[match.group(1)]
        repaired = source.rstrip() + added + "\n"
        try:
            tree = ast.parse(repaired, filename="<rlm-cell>", mode="exec")
        except SyntaxError as retry:
            error = retry
            continue
        return tree, repaired, f"[note: added the missing {added!r} at the end of the cell]"
    raise original


def _string_token_count(segment: str) -> int:
    try:
        return sum(1 for token in tokenize.generate_tokens(io.StringIO(segment).readline) if token.type == tokenize.STRING)
    except (tokenize.TokenError, SyntaxError):
        return -1


_QUOTED_ESCAPE = re.compile(r"\\(?:\\|['\"])")


def _raw_command_literal(source: str, node: ast.expr) -> str | None:
    """The value of a plain string literal read as if written raw, when that differs.

    A shell command is text for bash, which does its own escaping (and a heredoc's program its own):
    ``bash('''python - <<'PY' ... print('a\\nb') ...''')`` means the two characters ``\\n``, as
    Pi's bash tool would pass them. Python would turn them into a real newline and break the inner
    program, so a literal whose backslashes only make sense raw is read raw. Literals written raw,
    f-strings, concatenations, and ones that already escape backslashes or quotes (``\\\\``,
    ``\\'``) are left as written.
    """
    if not isinstance(node, ast.Constant) or not isinstance(node.value, str):
        return None
    segment = ast.get_source_segment(source, node)
    if not segment or "\\" not in segment:
        return None
    prefix = segment[: len(segment) - len(segment.lstrip("rRbBuUfF"))]
    if prefix.lower() not in ("", "u") or _QUOTED_ESCAPE.search(segment):
        return None
    if _string_token_count(segment) != 1:
        return None
    try:
        raw = ast.literal_eval("r" + segment[len(prefix):])
    except (SyntaxError, ValueError):
        return None
    return raw if isinstance(raw, str) and raw != node.value else None


class _RawShellLiterals(ast.NodeTransformer):
    def __init__(self, source: str) -> None:
        self._source = source

    def visit_Call(self, node: ast.Call) -> ast.AST:
        self.generic_visit(node)
        if isinstance(node.func, ast.Name) and node.func.id == "bash":
            if node.args:
                raw = _raw_command_literal(self._source, node.args[0])
                if raw is not None:
                    node.args[0] = ast.copy_location(ast.Constant(raw), node.args[0])
            for keyword in node.keywords:
                if keyword.arg == "command":
                    raw = _raw_command_literal(self._source, keyword.value)
                    if raw is not None:
                        keyword.value = ast.copy_location(ast.Constant(raw), keyword.value)
        return node


def _prepare_code(source: str) -> tuple[Any, str | None]:
    tree, text, note = _parse_cell(source)
    tree = _RawShellLiterals(text).visit(tree)
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        tree.body[-1] = ast.Assign(
            targets=[ast.Name(id="_rlm_result", ctx=ast.Store())],
            value=tree.body[-1].value,
        )
    ast.fix_missing_locations(tree)
    code = compile(tree, "<rlm-cell>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
    _remember_cell_source(code, text)
    return code, note


# Every cell is compiled as "<rlm-cell>", so a traceback cannot find its text by file name; the code objects of
# recent cells (functions they define included) map to their lines instead. A failing line shown in the traceback
# is one the model does not have to find again before fixing it.
_CELL_SOURCES: dict[int, tuple[Any, list[str]]] = {}
_CELL_SOURCE_ORDER: list[list[int]] = []
_CELL_SOURCES_KEPT = 200


def _remember_cell_source(code: Any, text: str) -> None:
    lines = text.splitlines()
    keys: list[int] = []
    pending = [code]
    while pending:
        current = pending.pop()
        _CELL_SOURCES[id(current)] = (current, lines)
        keys.append(id(current))
        pending.extend(const for const in current.co_consts if hasattr(const, "co_code"))
    _CELL_SOURCE_ORDER.append(keys)
    while len(_CELL_SOURCE_ORDER) > _CELL_SOURCES_KEPT:
        for key in _CELL_SOURCE_ORDER.pop(0):
            _CELL_SOURCES.pop(key, None)


def _frame_source_line(code: Any, lineno: int | None) -> str:
    if not lineno:
        return ""
    entry = _CELL_SOURCES.get(id(code))
    if entry is not None and entry[0] is code:
        lines = entry[1]
        return lines[lineno - 1].strip() if 0 < lineno <= len(lines) else ""
    with contextlib.suppress(Exception):
        return linecache.getline(code.co_filename, lineno).strip()
    return ""


_PREVIEW_BYTES = 8192
_TRUNCATION_MARKER = "\n... [truncated]"
_MARKER_BYTES = len(_TRUNCATION_MARKER.encode("utf-8"))


class _BoundedTextIO(io.TextIOBase):
    """Keep a UTF-8 prefix, never retaining more than the preview budget."""

    def __init__(self, limit: int = _PREVIEW_BYTES) -> None:
        super().__init__()
        self._limit = limit
        self._buffer = bytearray()
        self.truncated = False

    def writable(self) -> bool:
        return True

    def truncate_preview(self) -> None:
        self.truncated = True
        del self._buffer[self._limit - _MARKER_BYTES:]

    def write(self, text: str) -> int:
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not isinstance(text, str):
            raise TypeError("write() argument must be str")
        if not self.truncated:
            remaining = self._limit - len(self._buffer)
            # Slice before encoding so a huge write needs bounded temporary
            # space too. Escape lone surrogates to keep the preview valid UTF-8.
            encoded = text[:remaining + 1].encode("utf-8", errors="backslashreplace")
            self._buffer.extend(encoded[:remaining])
            if len(encoded) > remaining:
                self.truncate_preview()
        return len(text)

    def getvalue(self) -> str:
        # Drop a partial final code point rather than add a replacement glyph.
        return self._buffer.decode("utf-8", errors="ignore") + (
            _TRUNCATION_MARKER if self.truncated else ""
        )


def _output_budget() -> int:
    """Bytes of a cell's stdout or stderr kept for the model (ULTRON_RLM_OUTPUT_BYTES, default 20 KB)."""
    raw = os.environ.get("ULTRON_RLM_OUTPUT_BYTES", "")
    try:
        value = int(raw)
    except ValueError:
        return 20_000
    return value if value > 0 else 20_000


class _MiddleTextIO(io.TextIOBase):
    """Capture a stream keeping its head and tail (each half the budget) and count what was cut,
    so both the first error and the final summary of a long output survive (nano-rlm style)."""

    def __init__(self, limit: int) -> None:
        super().__init__()
        self._half = max(1, limit // 2)
        self._head = bytearray()
        self._tail = bytearray()
        self._total = 0

    def writable(self) -> bool:
        return True

    def write(self, text: str) -> int:
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not isinstance(text, str):
            raise TypeError("write() argument must be str")
        length = len(text)
        # Bound the temporary encoding of a huge write: only its ends can be kept.
        if length > 4 * self._half + 8:
            head_room = max(0, self._half - len(self._head))
            self._total += len(text.encode("utf-8", errors="backslashreplace"))
            front = text[:head_room].encode("utf-8", errors="backslashreplace")[:head_room]
            self._head.extend(front)
            self._tail = bytearray(text[-self._half:].encode("utf-8", errors="backslashreplace")[-self._half:])
            return length
        encoded = text.encode("utf-8", errors="backslashreplace")
        self._total += len(encoded)
        head_room = self._half - len(self._head)
        if head_room > 0:
            self._head.extend(encoded[:head_room])
            encoded = encoded[head_room:]
        if encoded:
            self._tail.extend(encoded)
            if len(self._tail) > 2 * self._half:
                del self._tail[: len(self._tail) - self._half]
        return length

    def getvalue(self) -> str:
        tail = bytes(self._tail[-self._half:])
        kept = len(self._head) + len(tail)
        head = self._head.decode("utf-8", errors="ignore")
        if kept >= self._total:
            return head + tail.decode("utf-8", errors="ignore")
        # Drop partial code points at the cut rather than add replacement glyphs.
        return (
            head
            + f"\n[... {self._total - kept} bytes truncated ...]\n"
            + tail.decode("utf-8", errors="ignore")
        )


def _text_preview(text: str) -> str:
    stream = _BoundedTextIO()
    stream.write(text)
    return stream.getvalue()


_PREVIEW_SECONDS = 2.0
_deadline_active = False
# Once a preview deadline fires, no further user __repr__/__str__ runs for that preview.
_deadline_expired = False


class _PreviewTimeout(BaseException):
    pass


@contextlib.contextmanager
def _preview_deadline(seconds: float = _PREVIEW_SECONDS):
    """Interrupt user __repr__/__str__ code that runs during preview rendering.

    Uses SIGALRM on the main thread. Code that swallows BaseException can still
    stall; host-side cell cancellation then terminates the kernel process group.
    """
    global _deadline_active, _deadline_expired
    usable = (
        not _deadline_active
        and hasattr(signal, "setitimer")
        and threading.current_thread() is threading.main_thread()
    )
    if not usable:
        yield
        return

    def on_alarm(_signum: int, _frame: Any) -> None:
        global _deadline_expired
        _deadline_expired = True
        raise _PreviewTimeout()

    previous = signal.signal(signal.SIGALRM, on_alarm)
    _deadline_active = True
    _deadline_expired = False
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)
        _deadline_active = False
        _deadline_expired = False


def _safe_repr(item: Any) -> str:
    if _deadline_expired:
        raise _PreviewTimeout()
    try:
        text = repr(item)
        if type(text) is not str:
            raise TypeError("__repr__ returned non-string")
        return text
    except _PreviewTimeout:
        raise
    except BaseException as error:
        return f"<{type(item).__qualname__} object; repr raised {type(error).__name__}>"


def _result_preview(value: Any, limit: int = _PREVIEW_BYTES, max_depth: int = 20) -> str:
    return _render_preview(value, limit, max_depth)[0]


def _render_preview(value: Any, limit: int = _PREVIEW_BYTES, max_depth: int = 20) -> tuple[str, bool]:
    """Render builtins incrementally without copying or traversing whole values.

    User-defined __repr__ methods run inside this worker, never on the host. Their
    failures become placeholders, their text is bounded, and under a preview
    deadline a stalled __repr__ ends the preview instead of the protocol.
    """
    stream = _BoundedTextIO(limit)
    active: set[int] = set()

    def render(item: Any, depth: int = 0) -> None:
        if stream.truncated:
            return
        kind = type(item)
        if kind in (str, bytes, bytearray):
            prefix = item[:limit]
            stream.write(repr(prefix))
            if len(item) > len(prefix):
                stream.truncate_preview()
            return
        if kind is int:
            # Avoid both enormous decimal strings and Python's conversion limit.
            digits = item.bit_length() * 30103 // 100000 + 1
            int_limit = getattr(sys, "get_int_max_str_digits", lambda: 0)()
            if digits + (item < 0) > limit or (int_limit and digits > int_limit):
                stream.write(f"<int with {item.bit_length()} bits>")
                stream.truncate_preview()
            else:
                stream.write(repr(item))
            return
        containers = (list, tuple, dict, set, frozenset, range, slice)
        is_exception = isinstance(item, BaseException) and kind.__repr__ is BaseException.__repr__
        if kind not in containers and not is_exception:
            stream.write(_safe_repr(item))
            return
        if id(item) in active:
            stream.write("{...}" if kind is dict else "[...]" if kind is list else "(...)")
            return
        if depth >= max_depth:
            stream.truncate_preview()
            return
        active.add(id(item))
        try:
            if kind is dict:
                opening, closing, values = "{", "}", iter(item)
            elif kind is list:
                opening, closing, values = "[", "]", iter(item)
            elif kind is tuple:
                opening, closing, values = "(", ")", iter(item)
            elif kind in (set, frozenset):
                if not item:
                    stream.write("set()" if kind is set else "frozenset()")
                    return
                opening = "{" if kind is set else "frozenset({"
                closing = "}" if kind is set else "})"
                values = iter(item)
            elif kind is range:
                opening, closing = "range(", ")"
                values = iter((item.start, item.stop) if item.step == 1 else (item.start, item.stop, item.step))
            elif kind is slice:
                opening, closing, values = "slice(", ")", iter((item.start, item.stop, item.step))
            else:
                stream.write(kind.__name__)
                opening, closing, values = "(", ")", iter(item.args)
            stream.write(opening)
            count = 0
            while not stream.truncated:
                try:
                    child = next(values)
                except StopIteration:
                    break
                if count:
                    stream.write(", ")
                render(child, depth + 1)
                if kind is dict and not stream.truncated:
                    stream.write(": ")
                    render(item[child], depth + 1)
                count += 1
            if kind is tuple and count == 1:
                stream.write(",")
            stream.write(closing)
        finally:
            active.remove(id(item))

    try:
        render(value)
    except _PreviewTimeout:
        stream.write(" <preview timed out>")
        stream.truncated = True
    except RecursionError:
        stream.write(" <preview recursion limit>")
        stream.truncated = True
    return stream.getvalue(), stream.truncated


def preview(value: Any, depth: int = 20, max_bytes: int = _PREVIEW_BYTES) -> dict[str, Any]:
    """Bounded, model-visible description of a value. The value itself is untouched."""
    if type(depth) is not int or not 1 <= depth <= 100:
        raise ValueError("preview depth must be an integer between 1 and 100")
    if type(max_bytes) is not int or not 64 <= max_bytes <= 1024 * 1024:
        raise ValueError("preview max_bytes must be an integer between 64 and 1048576")
    kind = type(value)
    sized = (str, bytes, bytearray, list, tuple, dict, set, frozenset, range)
    with _preview_deadline():
        text, truncated = _render_preview(value, max_bytes, depth)
    return {
        "type": _type_name(kind),
        "length": len(value) if kind in sized else None,
        "preview": text,
        "truncated": truncated,
        "max_bytes": max_bytes,
    }


# A cell's last expression is shown whole only up to these sizes; a larger value stays in the kernel
# (as `_`) and is shown by reference: its type, size, head, tail and digest (NOOA-style pass-by-reference).
_REFERENCE_CHARS = 2_000
_REFERENCE_ITEMS = 40
_REFERENCE_HEAD = 600
_REFERENCE_TAIL = 300
_REFERENCE_HINT = "The whole value is kept in the kernel as `_`: slice it, search it, or use preview(_) instead of showing it all."


def _digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()[:12]


def _items_preview(items: list[Any]) -> str:
    text, _truncated = _render_preview(items, _REFERENCE_HEAD * 2, 6)
    return text


def _frame_preview(value: Any) -> str | None:
    """A pandas DataFrame/Series or numpy array, described without importing those packages."""
    kind = type(value)
    name = kind.__name__
    if name in ("DataFrame", "Series") and hasattr(value, "shape") and hasattr(value, "head"):
        shape = "x".join(str(int(n)) for n in value.shape)
        lines = [f"<{name} {shape}, {_type_name(kind)}>"]
        if name == "DataFrame":
            lines.append("columns: " + _text_preview(", ".join(f"{col} ({dtype})" for col, dtype in value.dtypes.items())))
        lines.append("head:\n" + _text_preview(str(value.head(5))))
        if int(value.shape[0]) > 8:
            lines.append("tail:\n" + _text_preview(str(value.tail(3))))
        lines.append("It stays in the kernel as `_`: filter, aggregate or `.to_string()` a slice instead of showing it all.")
        return "\n".join(lines)
    if name == "ndarray" and hasattr(value, "shape") and hasattr(value, "dtype"):
        if int(value.size) <= _REFERENCE_ITEMS:
            return None
        shape = "x".join(str(int(n)) for n in value.shape)
        flat = value.ravel()
        return "\n".join([
            f"<ndarray {shape} dtype={value.dtype}, {_type_name(kind)}>",
            "head: " + _text_preview(str(flat[: _REFERENCE_ITEMS // 2])),
            "tail: " + _text_preview(str(flat[-(_REFERENCE_ITEMS // 4):])),
            _REFERENCE_HINT,
        ])
    return None


def _reference_preview(value: Any) -> str | None:
    """A bounded, by-reference description of a large value, or None when it is small enough to show whole."""
    kind = type(value)
    if kind is str:
        if len(value) <= _REFERENCE_CHARS:
            return None
        return "\n".join([
            f"<str: {len(value):,} chars, {value.count(chr(10)) + 1:,} lines, sha256 {_digest(value.encode('utf-8', 'surrogatepass'))}>",
            f"head: {value[:_REFERENCE_HEAD]!r}",
            f"tail: {value[-_REFERENCE_TAIL:]!r}",
            _REFERENCE_HINT,
        ])
    if kind in (bytes, bytearray):
        if len(value) <= _REFERENCE_CHARS:
            return None
        return "\n".join([
            f"<{kind.__name__}: {len(value):,} bytes, sha256 {_digest(bytes(value))}>",
            f"head: {bytes(value[: _REFERENCE_HEAD // 4])!r}",
            f"tail: {bytes(value[-(_REFERENCE_TAIL // 4):])!r}",
            _REFERENCE_HINT,
        ])
    if kind in (list, tuple):
        if len(value) <= _REFERENCE_ITEMS:
            return None
        kinds = sorted({type(item).__name__ for item in value[:200]})
        return "\n".join([
            f"<{kind.__name__}: {len(value):,} items; item types {', '.join(kinds)}>",
            f"head: {_items_preview(list(value[:10]))}",
            f"tail: {_items_preview(list(value[-5:]))}",
            _REFERENCE_HINT,
        ])
    if kind is dict:
        if len(value) <= _REFERENCE_ITEMS:
            return None
        keys = list(value)
        return "\n".join([
            f"<dict: {len(value):,} keys>",
            f"first keys: {_items_preview(keys[:10])}",
            f"last keys: {_items_preview(keys[-5:])}",
            f"first item: {_items_preview([keys[0], value[keys[0]]])}",
            _REFERENCE_HINT,
        ])
    if kind in (set, frozenset):
        if len(value) <= _REFERENCE_ITEMS:
            return None
        sample = []
        for item in value:
            sample.append(item)
            if len(sample) == 10:
                break
        return "\n".join([f"<{kind.__name__}: {len(value):,} items>", f"sample: {_items_preview(sample)}", _REFERENCE_HINT])
    try:
        return _frame_preview(value)
    except BaseException:
        return None


def _cell_result_text(value: Any) -> str:
    """The text shown for a cell's last expression: whole when small, by reference when large."""
    reference = _reference_preview(value)
    if reference is not None:
        return reference
    text, truncated = _render_preview(value)
    if truncated:
        return f"{text}\n{_REFERENCE_HINT}"
    return text


def _exception_message(error: BaseException) -> str:
    if _deadline_expired:
        return "<exception str() timed out>"
    try:
        if type(error).__str__ is BaseException.__str__:
            if not error.args:
                return ""
            if len(error.args) != 1:
                return _result_preview(error.args)
            value = error.args[0]
            if type(value) is str:
                return _text_preview(value)
            if type(value) in (int, bytes, bytearray, list, tuple, dict, set, frozenset, range, slice):
                return _result_preview(value)
            return _text_preview(str(value))
        if type(error) is KeyError and len(error.args) == 1:
            return _result_preview(error.args[0])
        return _text_preview(str(error))
    except BaseException:
        return "<exception str() failed>"


def _error_preview(error: BaseException) -> dict[str, Any]:
    stream = _BoundedTextIO()
    seen: set[int] = set()

    def render(current: BaseException, depth: int = 0) -> None:
        if stream.truncated or id(current) in seen:
            return
        if depth >= 20:
            stream.truncate_preview()
            return
        seen.add(id(current))
        cause = current.__cause__
        context = current.__context__
        if cause is not None:
            render(cause, depth + 1)
            stream.write("\nThe above exception was the direct cause of the following exception:\n\n")
        elif context is not None and not current.__suppress_context__:
            render(context, depth + 1)
            stream.write("\nDuring handling of the above exception, another exception occurred:\n\n")
        if stream.truncated:
            return
        if current.__traceback__ is not None:
            stream.write("Traceback (most recent call last):\n")
            for frame, lineno in traceback.walk_tb(current.__traceback__):
                code = frame.f_code
                if code.co_name == "execute_cell" and code.co_filename == __file__:
                    continue  # the kernel's own frame around every cell
                stream.write('  File "')
                stream.write(code.co_filename)
                stream.write(f'", line {lineno}, in ')
                stream.write(code.co_name)
                stream.write("\n")
                line = _frame_source_line(code, lineno)
                if line:
                    stream.write("    ")
                    stream.write(line[:300])
                    stream.write("\n")
                if stream.truncated:
                    return
        stream.write(type(current).__name__)
        stream.write(": ")
        stream.write(_exception_message(current))
        stream.write("\n")

    render(error)
    return {
        "ename": _text_preview(type(error).__name__),
        "evalue": _exception_message(error),
        "traceback": stream.getvalue().splitlines(),
    }


_STATE = RuntimeState()


async def execute_cell(request_id: str, source: str) -> None:
    async with _STATE.execution_lock:
        budget = _output_budget()
        stdout = _MiddleTextIO(budget)
        stderr = _MiddleTextIO(budget)
        _STATE.namespace.pop("_rlm_result", None)
        try:
            compiled, note = _prepare_code(source)
            if note is not None:
                stdout.write(note + "\n")
            _arm_cpu_limit()
            _STATE.cell_active = True
            try:
                with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                    agent_class_api.begin_cell(source)
                    try:
                        result = eval(compiled, _STATE.namespace, _STATE.namespace)
                        if asyncio.iscoroutine(result):
                            await result
                    finally:
                        agent_class_api.end_cell()
                if "_rlm_result" in _STATE.namespace:
                    value = _STATE.namespace.pop("_rlm_result")
                    if value is None:
                        # As in IPython, a None expression (a bare print(...) call) shows nothing.
                        result_text = ""
                    else:
                        # The value stays reachable as `_`, so a by-reference preview can be followed up.
                        _STATE.namespace["_"] = value
                        with _preview_deadline():
                            result_text = _cell_result_text(value)
                else:
                    result_text = ""
            finally:
                _STATE.cell_active = False
            emit("stdout", id=request_id, text=stdout.getvalue())
            emit("stderr", id=request_id, text=stderr.getvalue())
            emit("result", id=request_id, result=result_text)
            emit("done", id=request_id, status="ok", flush=_flush_raw_output(request_id))
        except BaseException as error:
            try:
                emit("stdout", id=request_id, text=stdout.getvalue())
                emit("stderr", id=request_id, text=stderr.getvalue())
                try:
                    with _preview_deadline():
                        details = _error_preview(error)
                except BaseException:
                    details = {"ename": _text_preview(type(error).__name__), "evalue": "<error preview failed>", "traceback": []}
                if isinstance(error, MemoryError) and _MAX_MEMORY_MB > 0:
                    note = _memory_limit_note()
                    details["evalue"] = f"{details['evalue']} ({note})" if details["evalue"] else note
                emit("error", id=request_id, **details)
                emit("done", id=request_id, status="error", flush=_flush_raw_output(request_id))
            except BaseException as secondary:
                # The cell cannot be reported in-band (e.g. no memory left to encode it): exit with a code
                # the host maps to a clear error, rather than leaving the cell pending forever.
                _exit_for(error, secondary)


def _exit_for(*errors: BaseException) -> None:
    code = _EXIT_INTERNAL
    if any(isinstance(error, MemoryError) for error in errors):
        code = _EXIT_MEMORY
    elif any(isinstance(error, RlmCpuLimitExceeded) for error in errors):
        code = _EXIT_CPU
    for stream in (_PROTOCOL_OUT, sys.__stdout__, sys.__stderr__):
        try:
            stream.flush()
        except BaseException:
            pass
    os._exit(code)


async def handle_request(frame: dict[str, Any]) -> None:
    request_type = frame.get("request")
    request_id = frame.get("id")
    if request_type == "execute":
        asyncio.create_task(execute_cell(request_id, str(frame.get("code", ""))))
    elif request_type == "host_reply":
        _STATE.bridge.resolve(str(frame.get("id")), frame.get("payload"), frame.get("error"))
    elif request_type == "snapshot":
        try:
            result = save_snapshot(str(frame.get("path")))
            emit("done", id=request_id, status="ok", snapshot=result)
        except Exception as error:
            emit("done", id=request_id, status="error", error=_exception_message(error))
    elif request_type == "restore":
        try:
            result = restore_snapshot(str(frame.get("path")), frame.get("content_sha256"))
            emit("done", id=request_id, status="ok", restore=result)
        except Exception as error:
            emit("done", id=request_id, status="error", error=_exception_message(error))
    elif request_type == "reset_scratch":
        async with _STATE.execution_lock:
            _STATE.reset_scratch()
        emit("done", id=request_id, status="ok")
    elif request_type == "list_names":
        emit("done", id=request_id, status="ok", names=sorted(name for name in _STATE.namespace if not name.startswith("__")))
    elif request_type == "shutdown":
        emit("done", id=request_id, status="ok")
        raise SystemExit(0)
    else:
        detail = _text_preview(request_type) if isinstance(request_type, str) else _result_preview(request_type)
        emit("done", id=request_id, status="error", error=_text_preview("unknown request: " + detail))


async def main() -> None:
    # Output before this marker (interpreter warnings, for example) is startup diagnostics, not a cell's output.
    _flush_raw_output("ready")
    emit("ready", protocol=1, pid=os.getpid())
    while True:
        line = await asyncio.to_thread(_PROTOCOL_IN.readline)
        if not line:
            return
        try:
            frame = json.loads(line.decode("utf-8"))
            if isinstance(frame, dict):
                await handle_request(frame)
        except SystemExit:
            return
        except Exception as error:
            emit("error", **_error_preview(error))


if __name__ == "__main__":
    _open_protocol()
    _become_subreaper()
    _apply_resource_limits()
    try:
        asyncio.run(main())
    except (MemoryError, RlmCpuLimitExceeded) as fatal:
        _exit_for(fatal)
