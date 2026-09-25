from __future__ import annotations

import ast
import asyncio
import contextlib
import io
import json
import os
import sys
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

try:
    import dill as _dill
except Exception:
    _dill = None


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

async def bash(command: str) -> dict[str, Any]:
    if not isinstance(command, str) or not command.strip():
        raise ValueError("bash command must be a non-empty string")
    return await _STATE.bridge.request("bash", {"command": command})


class RuntimeState:
    def __init__(self) -> None:
        self.bridge = HostBridge()
        self.execution_lock = asyncio.Lock()
        self.snapshot_path: Path | None = None
        self.namespace: dict[str, Any] = {"__name__": "__main__"}
        self.namespace["rlm"] = RLMNamespace(self.bridge)
        self.namespace["agent_message"] = AgentMessages(self.bridge)
        self.namespace["jev"] = JevNamespace(self.bridge)
        self.namespace["background"] = BackgroundNamespace(self.bridge)
        self.namespace["bash"] = bash
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
        # Declared instance state survives reset_scratch; every other name is invocation scratch.
        self.namespace["state"] = {}
        self.bindings = {name: value for name, value in self.namespace.items() if name != "state"}

    def reset_scratch(self) -> None:
        state = self.namespace.get("state", {})
        self.namespace.clear()
        self.namespace.update(self.bindings)
        self.namespace["state"] = state

_STATE = RuntimeState()


def emit(event: str, **fields: Any) -> None:
    payload = {"event": event, **fields}
    sys.__stdout__.write(json.dumps(payload, default=str, separators=(",", ":")) + "\n")
    sys.__stdout__.flush()


def _snapshot_value(value: Any) -> Any:
    if isinstance(value, (str, int, float, bool)) or value is None:
        return value
    if isinstance(value, (list, tuple)) and all(_snapshot_value(v) is not _UNSERIALIZABLE for v in value):
        return [_snapshot_value(v) for v in value]
    if isinstance(value, dict):
        converted = {}
        for key, item in value.items():
            if not isinstance(key, str):
                return _UNSERIALIZABLE
            converted[key] = _snapshot_value(item)
            if converted[key] is _UNSERIALIZABLE:
                return _UNSERIALIZABLE
        return converted
    return _UNSERIALIZABLE


class _Unserializable:
    pass


_UNSERIALIZABLE = _Unserializable()


def _persistable_namespace() -> tuple[dict[str, Any], list[str]]:
    serializable: dict[str, Any] = {}
    failed: list[str] = []
    excluded = {"rlm", "agent_message", "jev", "background", "bash", "SpawnHandle", "agents", "workflows", "memory", "refinements"}
    for name, value in _STATE.namespace.items():
        if name.startswith("__") or name in excluded:
            continue
        if _dill is not None:
            try:
                _dill.dumps(value)
                serializable[name] = value
                continue
            except Exception:
                pass
        converted = _snapshot_value(value)
        if converted is _UNSERIALIZABLE:
            failed.append(name)
        else:
            serializable[name] = converted
    return serializable, failed


def save_snapshot(path: str) -> dict[str, Any]:
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    serializable, failed = _persistable_namespace()
    payload = {"version": 1, "names": serializable, "skipped": failed}
    if _dill is not None:
        with target.open("wb") as stream:
            _dill.dump(payload, stream)
    else:
        target.write_text(json.dumps(payload), encoding="utf-8")
    _STATE.snapshot_path = target
    return {"saved": sorted(serializable), "skipped": failed, "serializer": "dill" if _dill is not None else "json"}


def restore_snapshot(path: str) -> dict[str, Any]:
    target = Path(path)
    if not target.exists():
        return {"restored": [], "missing": True}
    try:
        if _dill is not None:
            with target.open("rb") as stream:
                data = _dill.load(stream)
        else:
            data = json.loads(target.read_text(encoding="utf-8"))
    except Exception:
        data = json.loads(target.read_text(encoding="utf-8"))
    restored = []
    for name, value in (data.get("names") or {}).items():
        if isinstance(name, str):
            _STATE.namespace[name] = value
            restored.append(name)
    _STATE.snapshot_path = target
    return {"restored": sorted(restored), "missing": False}


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

    def __init__(self) -> None:
        super().__init__()
        self._buffer = bytearray()
        self.truncated = False

    def writable(self) -> bool:
        return True

    def truncate_preview(self) -> None:
        self.truncated = True
        del self._buffer[_PREVIEW_BYTES - _MARKER_BYTES:]

    def write(self, text: str) -> int:
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not isinstance(text, str):
            raise TypeError("write() argument must be str")
        if not self.truncated:
            remaining = _PREVIEW_BYTES - len(self._buffer)
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


def _text_preview(text: str) -> str:
    stream = _BoundedTextIO()
    stream.write(text)
    return stream.getvalue()


def _result_preview(value: Any) -> str:
    """Render builtins incrementally without copying or traversing whole values.

    User-defined __repr__ methods still run normally. Their returned text is
    bounded, but arbitrary code inside those methods cannot be memory-limited.
    """
    stream = _BoundedTextIO()
    active: set[int] = set()

    def render(item: Any, depth: int = 0) -> None:
        if stream.truncated:
            return
        kind = type(item)
        if kind in (str, bytes, bytearray):
            prefix = item[:_PREVIEW_BYTES]
            stream.write(repr(prefix))
            if len(item) > len(prefix):
                stream.truncate_preview()
            return
        if kind is int:
            # Avoid both enormous decimal strings and Python's conversion limit.
            digits = item.bit_length() * 30103 // 100000 + 1
            int_limit = getattr(sys, "get_int_max_str_digits", lambda: 0)()
            if digits + (item < 0) > _PREVIEW_BYTES or (int_limit and digits > int_limit):
                stream.write(f"<int with {item.bit_length()} bits>")
                stream.truncate_preview()
            else:
                stream.write(repr(item))
            return
        containers = (list, tuple, dict, set, frozenset, range, slice)
        is_exception = isinstance(item, BaseException) and kind.__repr__ is BaseException.__repr__
        if kind not in containers and not is_exception:
            stream.write(repr(item))
            return
        if id(item) in active:
            stream.write("{...}" if kind is dict else "[...]" if kind is list else "(...)")
            return
        if depth >= 20:
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

    render(value)
    return stream.getvalue()


def _exception_message(error: BaseException) -> str:
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


async def execute_cell(request_id: str, source: str) -> None:
    async with _STATE.execution_lock:
        stdout = _BoundedTextIO()
        stderr = _BoundedTextIO()
        _STATE.namespace.pop("_rlm_result", None)
        try:
            compiled = _prepare_code(source)
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                result = eval(compiled, _STATE.namespace, _STATE.namespace)
                if asyncio.iscoroutine(result):
                    await result
            if "_rlm_result" in _STATE.namespace:
                result_text = _result_preview(_STATE.namespace.pop("_rlm_result"))
            else:
                result_text = ""
            emit("stdout", id=request_id, text=stdout.getvalue())
            emit("stderr", id=request_id, text=stderr.getvalue())
            emit("result", id=request_id, result=result_text)
            emit("done", id=request_id, status="ok")
        except BaseException as error:
            emit("stdout", id=request_id, text=stdout.getvalue())
            emit("stderr", id=request_id, text=stderr.getvalue())
            emit("error", id=request_id, **_error_preview(error))
            emit("done", id=request_id, status="error")


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
            result = restore_snapshot(str(frame.get("path")))
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
    asyncio.run(main())
