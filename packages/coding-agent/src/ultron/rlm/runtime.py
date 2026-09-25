from __future__ import annotations

import ast
import asyncio
import base64
import contextlib
import hashlib
import io
import json
import math
import os
import signal
import sys
import threading
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

@dataclass
class SpawnHandle:
    rlm_child_id: str
    name: str
    session_dir: str
    model: str
    timeout_ms: int
    parent_branch_anchor: str
    def __repr__(self) -> str:
        return f"SpawnHandle(name={self.name!r}, rlm_child_id={self.rlm_child_id!r})"


class HostBridge:
    def __init__(self) -> None:
        self._host_requests: dict[str, asyncio.Future[Any]] = {}
        self._counter = 0
        self._write_lock = asyncio.Lock()

    async def request(self, request_type: str, payload: dict[str, Any] | None = None) -> Any:
        self._counter += 1
        request_id = f"host-{self._counter}"
        loop = asyncio.get_running_loop()
        future: asyncio.Future[Any] = loop.create_future()
        self._host_requests[request_id] = future
        async with self._write_lock:
            sys.__stdout__.write(json.dumps({
                "event": "host_request",
                "id": request_id,
                "type": request_type,
                "payload": payload or {},
            }, separators=(",", ":")) + "\n")
            sys.__stdout__.flush()
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
    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    async def spawn(self, prompt: str, **kwargs: Any) -> SpawnHandle:
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("rlm.spawn prompt must be a non-empty string")
        name = kwargs.get("name")
        if not isinstance(name, str) or not name.strip():
            raise ValueError("rlm.spawn name must be a non-empty string")
        allowed = {"name", "model", "thinking", "timeout_ms"}
        unknown = sorted(set(kwargs) - allowed)
        if unknown:
            raise TypeError(f"rlm.spawn unknown options: {', '.join(unknown)}")
        timeout_ms = kwargs.get("timeout_ms")
        if timeout_ms is not None and (isinstance(timeout_ms, bool) or not isinstance(timeout_ms, (int, float)) or timeout_ms < 1):
            raise ValueError("rlm.spawn timeout_ms must be a positive number")
        result = await self._bridge.request("rlm.spawn", {
            "prompt": prompt,
            "kwargs": kwargs,
        })
        return SpawnHandle(**result)

    async def list_subagents(self) -> list[dict[str, Any]]:
        result = await self._bridge.request("rlm.list_subagents")
        return result.get("subagents", []) if isinstance(result, dict) else []

    async def collect(self, selectors: list[str] | None = None, timeout_ms: int = 0) -> list[dict[str, Any]]:
        result = await self._bridge.request("rlm.collect", {
            "selectors": selectors or [],
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

class BackgroundNamespace:
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
    """

    output: str
    exit_code: int | None
    timed_out: bool
    cancelled: bool
    truncated: bool
    full_output_path: str | None

    def __new__(cls, output: str, exit_code: int | None, *, timed_out: bool = False, cancelled: bool = False,
                truncated: bool = False, full_output_path: str | None = None, timeout: float | None = None) -> "BashOutput":
        text = output.strip()
        if timed_out:
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
        return self

    @property
    def ok(self) -> bool:
        return self.exit_code == 0 and not self.timed_out and not self.cancelled

    def __getitem__(self, key: Any) -> Any:
        if isinstance(key, str):
            if key in ("output", "exit_code", "timed_out", "cancelled", "truncated", "full_output_path", "ok"):
                return getattr(self, key)
            raise KeyError(key)
        return str.__getitem__(self, key)

    def get(self, key: str, default: Any = None) -> Any:
        try:
            return self[key]
        except KeyError:
            return default


async def bash(command: str, timeout: float | None = None) -> BashOutput:
    """Run a shell command in the working directory and return its output as a string.

    Args:
        command: The command, run by the user's shell (bash).
        timeout: Seconds before the command is killed (default: no limit).

    Returns:
        A BashOutput: stdout and stderr combined, with "[exit code N]" appended when the
        command failed. Its .exit_code and .ok attributes carry the status.
    """
    if not isinstance(command, str) or not command.strip():
        raise ValueError("bash command must be a non-empty string")
    if timeout is not None and (isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or timeout <= 0):
        raise ValueError("bash timeout must be a positive number of seconds")
    payload: dict[str, Any] = {"command": command}
    if timeout is not None:
        payload["timeout"] = timeout
    result = await _STATE.bridge.request("bash", payload)
    if not isinstance(result, dict):
        raise RuntimeError("bash: the host returned no result")
    output = str(result.get("output") or "")
    truncated = bool(result.get("truncated"))
    path = result.get("full_output_path")
    if truncated and isinstance(path, str) and path:
        # The host keeps only the tail; the whole output is in its spill file.
        with contextlib.suppress(OSError):
            with open(path, "rb") as handle:
                data = handle.read(_BASH_MAX_OUTPUT_BYTES + 1)
            if len(data) <= _BASH_MAX_OUTPUT_BYTES:
                output = data.decode("utf-8", errors="replace")
                truncated = False
    exit_code = result.get("exit_code")
    return BashOutput(
        output,
        exit_code if isinstance(exit_code, int) else None,
        timed_out=bool(result.get("timed_out")),
        cancelled=bool(result.get("cancelled")),
        truncated=truncated,
        full_output_path=path if isinstance(path, str) else None,
        timeout=timeout,
    )


async def edit(path: str, old_str: str, new_str: str) -> str:
    """Replace a unique string in a file (nano-rlm's edit skill).

    Args:
        path: File path, relative to the working directory or absolute.
        old_str: Exact string to find; it must appear exactly once in the file.
        new_str: Replacement string.

    Returns:
        A confirmation message. Raises FileNotFoundError when the file is missing and
        ValueError when old_str is absent or appears more than once.
    """
    filepath = Path(path)
    if not filepath.is_absolute():
        filepath = Path.cwd() / filepath
    if not filepath.exists():
        raise FileNotFoundError(f"{path} not found")
    content = filepath.read_text()
    count = content.count(old_str)
    if count != 1:
        raise ValueError(f"old_str must appear exactly once in {path} (found {count})")
    filepath.write_text(content.replace(old_str, new_str, 1))
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
        self.namespace: dict[str, Any] = {"__name__": "__main__"}
        self.namespace["rlm"] = RLMNamespace(self.bridge)
        self.namespace["agent_message"] = AgentMessages(self.bridge)
        self.namespace["jev"] = JevNamespace(self.bridge)
        self.namespace["background"] = BackgroundNamespace(self.bridge)
        self.namespace["bash"] = bash
        self.namespace["edit"] = edit
        self.namespace["SpawnHandle"] = SpawnHandle
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

def emit(event: str, **fields: Any) -> None:
    payload = {"event": event, **fields}
    sys.__stdout__.write(json.dumps(payload, default=str, separators=(",", ":")) + "\n")
    sys.__stdout__.flush()


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


def _prepare_code(source: str) -> Any:
    tree = ast.parse(source, filename="<rlm-cell>", mode="exec")
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        tree.body[-1] = ast.Assign(
            targets=[ast.Name(id="_rlm_result", ctx=ast.Store())],
            value=tree.body[-1].value,
        )
    ast.fix_missing_locations(tree)
    return compile(tree, "<rlm-cell>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)


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
                stream.write('  File "')
                stream.write(frame.f_code.co_filename)
                stream.write(f'", line {lineno}, in ')
                stream.write(frame.f_code.co_name)
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
            compiled = _prepare_code(source)
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
            emit("done", id=request_id, status="ok")
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
                emit("done", id=request_id, status="error")
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
    try:
        sys.__stdout__.flush()
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
    emit("ready", protocol=1, pid=os.getpid())
    while True:
        line = await asyncio.to_thread(sys.stdin.readline)
        if not line:
            return
        try:
            frame = json.loads(line)
            if isinstance(frame, dict):
                await handle_request(frame)
        except SystemExit:
            return
        except Exception as error:
            emit("error", **_error_preview(error))


if __name__ == "__main__":
    _become_subreaper()
    _apply_resource_limits()
    try:
        asyncio.run(main())
    except (MemoryError, RlmCpuLimitExceeded) as fatal:
        _exit_for(fatal)
