"""Durable many-agent APIs for the Rust host: persistent children, durable partial output, peer messages with a
registry of what each agent holds, and the session's shared task board and notes.

`rlm.spawn(..., keep=True)` keeps a child alive after its first answer (its context, files and kernel survive
restarts); `await rlm.ask(child, question)` asks it a follow-up, so your context grows only by the question and the
answer. `await rlm.partial(text)` records output that survives a crash of this cell. `swarm`, `board` and `notes`
are the session's shared world: every agent of the session reads and writes the same durable records.
Hosts without these requests (the TypeScript host) answer them with an error.
"""

from __future__ import annotations

import copy
from typing import Any


def _payload(**fields: Any) -> dict[str, Any]:
    return {key: value for key, value in fields.items() if value is not None}


class _KeepBridge:
    """Forwards requests, marking `rlm.spawn` as a persistent spawn."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def request(self, request_type: str, payload: dict[str, Any] | None = None) -> Any:
        if request_type == "rlm.spawn":
            payload = {**(payload or {}), "keep": True}
        return await self._bridge.request(request_type, payload)


class Swarm:
    """Agents of this session: who holds what, and durable messages between them.

    `await swarm.register(topics=[...], files=[...], work="...", summary="...")` describes what you hold (merged
    into your entry; children are registered when they start). `await swarm.route("question")` ranks the agents
    whose topics, files or recent work match, so you can `rlm.ask` the one with the context.
    `await swarm.send("name", "text", wake=True)` leaves a durable message (and wakes the agent with it as a
    follow-up); `await swarm.inbox()` returns your unread messages and marks them read."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def register(self, name: str | None = None, *, topics: Any = None, files: Any = None,
                       work: str | None = None, summary: str | None = None) -> dict[str, Any]:
        return await self._bridge.request("swarm.register", _payload(
            name=name, topics=list(topics) if topics is not None else None,
            files=list(files) if files is not None else None, work=work, summary=summary))

    async def agents(self) -> dict[str, Any]:
        return await self._bridge.request("swarm.agents", {})

    async def route(self, query: str) -> list[dict[str, Any]]:
        return await self._bridge.request("swarm.route", {"query": query})

    async def send(self, to: str, text: str, *, wake: bool = False) -> dict[str, Any]:
        return await self._bridge.request("swarm.send", {"to": to, "text": text, "wake": wake})

    async def inbox(self, *, all: bool = False, mark: bool = True) -> list[dict[str, Any]]:
        return await self._bridge.request("swarm.inbox", {"all": all, "mark": mark})


class Board:
    """The session's task board: `await board.add("title")`, `board.claim(id)` (refused when another agent holds
    it), `board.update(id, status="done", note="...")`, `board.list(status=None)`."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def add(self, title: str, detail: str | None = None) -> dict[str, Any]:
        return await self._bridge.request("board.add", _payload(title=title, detail=detail))

    async def claim(self, task_id: int) -> dict[str, Any]:
        return await self._bridge.request("board.claim", {"id": task_id})

    async def update(self, task_id: int, *, status: str | None = None, note: str | None = None) -> dict[str, Any]:
        return await self._bridge.request("board.update", _payload(id=task_id, status=status, note=note))

    async def list(self, status: str | None = None) -> list[dict[str, Any]]:
        return await self._bridge.request("board.list", _payload(status=status))


class Notes:
    """The session's shared notes: `await notes.read()` -> `{"text", "version"}`, `notes.append("line")`, and
    `notes.write(text, expect_version=v)`, refused when someone changed them since version `v`."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def read(self) -> dict[str, Any]:
        return await self._bridge.request("notes.read", {})

    async def append(self, text: str) -> dict[str, Any]:
        return await self._bridge.request("notes.append", {"text": text})

    async def write(self, text: str, expect_version: int | None = None) -> dict[str, Any]:
        return await self._bridge.request("notes.write", _payload(text=text, expect_version=expect_version))


def install(rlm_class: Any, namespace: dict[str, Any], bridge: Any) -> None:
    """Add `rlm.ask`, `rlm.partial` and `spawn(keep=True)` to the `rlm` class, and `swarm`, `board`, `notes`."""
    original_spawn = rlm_class.spawn

    async def spawn(self: Any, prompt: str, **kwargs: Any) -> Any:
        keep = kwargs.pop("keep", False)
        if keep not in (True, False):
            raise ValueError("rlm.spawn keep must be True or False")
        if not keep:
            return await original_spawn(self, prompt, **kwargs)
        persistent = copy.copy(self)
        persistent._bridge = _KeepBridge(self._bridge)
        return await original_spawn(persistent, prompt, **kwargs)

    spawn.__doc__ = (original_spawn.__doc__ or "") + (
        "\n\n        keep=True keeps the child alive after its first answer (it survives restarts); ask it follow-ups"
        " with `await rlm.ask(handle, question)`.")

    async def ask(self: Any, child: Any, question: str) -> dict[str, Any]:
        """Ask a persistent child (spawned with keep=True) a follow-up question and wait for its answer:
        `{"id", "status": "succeeded", "value": answer}` or `{"status": "failed", "error": ...}`."""
        child_id = getattr(child, "rlm_child_id", child)
        if not isinstance(question, str) or not question.strip():
            raise ValueError("rlm.ask question must be a non-empty string")
        return await self._bridge.request("rlm.ask", {"child": child_id, "question": question})

    async def partial(self: Any, text: str) -> None:
        """Record output of this cell that survives a crash: if the process dies while the cell runs, the
        interrupted result carries it (the cell is never run again)."""
        await self._bridge.request("cell.output", {"text": str(text)})

    rlm_class.spawn = spawn
    rlm_class.ask = ask
    rlm_class.partial = partial
    namespace["swarm"] = Swarm(bridge)
    namespace["board"] = Board(bridge)
    namespace["notes"] = Notes(bridge)
