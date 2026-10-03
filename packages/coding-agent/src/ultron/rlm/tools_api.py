"""Extension tools as pre-imported async skills (Prime Intellect's RLM harness exposes MCP tools the same way).

Tools that Pi extensions register (the built-in ``mcp`` gateway among them) run in the Ultron host with
their real tool context; the kernel calls them through ``tools.*`` host requests:

    await tools.list()                       # [{name, description}]
    await tools.describe("mcp")              # {name, description, parameters (JSON schema)}
    r = await tools.call("probe", {"x": 1})  # ToolResult: the text, plus .details
    r = await tools.probe(x=1)               # the same, by attribute

``mcp`` is a thin namespace over Ultron's MCP gateway tool (src/extensions/mcp), which connects the servers of
``mcp.json`` on first use:

    await mcp.servers()                                    # [{name, status, toolCount, ...}]
    await mcp.tools("exa-agent")                           # tool names of one server (all servers without one)
    await mcp.describe("exa-agent_exa_agent_create_run")   # {name, server, description, parameters}
    r = await mcp.call("exa-agent_exa_agent_create_run", query="...", effort="high")
    r = await mcp.exa_agent.exa_agent_create_run(query="...")   # server.tool attribute access

A call still running after ``yield_after`` seconds (default ULTRON_TOOL_YIELD_AFTER, else ULTRON_BASH_YIELD_AFTER,
else 30) keeps running in the host: a plain call then returns a ToolResult with ``.running`` True and ``.call``
its ToolCall handle, and the end arrives as a ``<runtime_event kind="tool_done">``. ``yield_after=None`` waits
however long it takes; ``yield_after=s`` returns a ToolCall handle after at most ``s`` seconds (0: at once).
"""

from __future__ import annotations

import contextlib
import json
import math
import os
import re
from typing import Any

_DEFAULT_TOOL_YIELD_AFTER = 30.0
_MAX_WAIT_SECONDS = 3600


class _Default:
    def __repr__(self) -> str:
        return "default"


_DEFAULT = _Default()


def _events_enabled() -> bool:
    return os.environ.get("ULTRON_ASYNC_EVENTS", "").strip().lower() not in ("off", "0", "false", "no")


def _default_yield_after() -> float | None:
    """ULTRON_TOOL_YIELD_AFTER (else ULTRON_BASH_YIELD_AFTER) seconds, default 30; 0 or "off" blocks."""
    raw = (os.environ.get("ULTRON_TOOL_YIELD_AFTER", "").strip() or os.environ.get("ULTRON_BASH_YIELD_AFTER", "").strip()).lower()
    if raw == "":
        return _DEFAULT_TOOL_YIELD_AFTER
    if raw in ("off", "none", "false", "no"):
        return None
    try:
        value = float(raw)
    except ValueError:
        return _DEFAULT_TOOL_YIELD_AFTER
    if not math.isfinite(value) or value < 0:
        return _DEFAULT_TOOL_YIELD_AFTER
    return None if value == 0 else min(value, _MAX_WAIT_SECONDS)


class ToolError(RuntimeError):
    """An extension tool call failed or was cancelled (``.name``, ``.call_id``, ``.details``, ``.status``)."""

    def __init__(self, message: str, *, name: str | None = None, call_id: str | None = None,
                 details: Any = None, status: str | None = None) -> None:
        super().__init__(message)
        self.name = name
        self.call_id = call_id
        self.details = details
        self.status = status


class McpError(ToolError):
    """The MCP gateway reported an error (unknown tool, server not connected, auth required, a tool error)."""


class ToolResult(str):
    """The text of a finished tool call, as a string, with the rest as attributes.

    .details (the tool's structured details, or None), .name, .id, .ok, .truncated (text cut in the middle past
    ULTRON_TOOL_RESULT_BYTES, 256 KiB), .elapsed_seconds, and ``.json()`` to parse the text as JSON. A call still running when a
    plain call stopped waiting has .running True and .call (its ToolCall); the text then says so.
    """

    details: Any
    name: str
    id: str
    ok: bool
    running: bool
    truncated: bool
    elapsed_seconds: float
    call: "ToolCall | None"
    output: str

    def __new__(cls, text: str, *, details: Any = None, name: str = "", call_id: str = "", ok: bool = True,
                truncated: bool = False, elapsed_seconds: float = 0.0, call: "ToolCall | None" = None,
                waited: float | None = None) -> "ToolResult":
        running = call is not None and call.running
        shown = text
        if running:
            waited_text = f" after {waited:g} s" if waited is not None else ""
            shown = (
                f"[{name} still running as call {call.id}{waited_text}; its completion will arrive as a runtime "
                "event, or `await <result>.call.result()` to wait]"
                if _events_enabled()
                else f"[{name} still running as call {call.id}{waited_text}; `await <result>.call.result()` waits for it]"
            )
        self = super().__new__(cls, shown)
        self.output = text
        self.details = details
        self.name = name
        self.id = call_id
        self.ok = ok and not running
        self.running = running
        self.truncated = truncated
        self.elapsed_seconds = elapsed_seconds
        self.call = call
        return self

    def json(self) -> Any:
        """Parse the text as JSON: the whole text, else its first JSON value (an MCP gateway may append a
        ``structuredContent:`` copy after the text), else that structured copy. A fenced ```json block is unwrapped."""
        text = self.output.strip()
        fenced = re.match(r"^```(?:json)?\s*\n(.*)\n```$", text, re.S)
        if fenced:
            text = fenced.group(1).strip()
        try:
            return json.loads(text)
        except json.JSONDecodeError as error:
            first_error = error
        with contextlib.suppress(json.JSONDecodeError):
            return json.JSONDecoder().raw_decode(text)[0]
        marker = "structuredContent:\n"
        if marker in text:
            with contextlib.suppress(json.JSONDecodeError):
                return json.loads(text.split(marker, 1)[1])
        raise first_error


class ToolCall:
    """An extension tool call owned by the Ultron host (``tools.call(..., yield_after=s)``).

    It keeps running after the cell ends and across kernel restarts; an Esc abort of the turn that started it or
    ``await call.cancel()`` stops it. When it ends while you are not waiting on it, a
    ``<runtime_event kind="tool_done">`` message arrives on its own: do not poll.

    Attributes: .id, .name, .label, .status, .running, .ok, .text, .details, .truncated, .elapsed_seconds, .error.
    """

    def __init__(self, bridge: Any, data: Any) -> None:
        self._bridge = bridge
        self._update(data)

    def _update(self, data: Any) -> None:
        if not isinstance(data, dict) or not isinstance(data.get("id"), str):
            raise RuntimeError("tools: the host returned no call")
        self.id: str = data["id"]
        self.name: str = str(data.get("name") or "")
        self.label: str = str(data.get("label") or self.name)
        self.status: str = str(data.get("status") or "")
        self.running: bool = bool(data.get("running"))
        self.ok: bool = bool(data.get("ok"))
        text = data.get("text")
        self.text: str | None = text if isinstance(text, str) else None
        self.details: Any = data.get("details")
        self.truncated: bool = bool(data.get("truncated"))
        self.elapsed_seconds: float = float(data.get("elapsed_seconds") or 0)
        error = data.get("error")
        self.error: str | None = error if isinstance(error, str) else None

    def _as_result(self, waited: float | None = None) -> ToolResult:
        if self.running:
            return ToolResult("", name=self.label, call_id=self.id, call=self, waited=waited)
        if self.status != "completed" or self.text is None:
            detail = self.error or self.status
            raise ToolError(f"{self.label} {self.status}: {detail}" if self.status != "failed" else f"{self.label} failed: {detail}",
                            name=self.name, call_id=self.id, details=self.details, status=self.status)
        if self.name == "mcp" and isinstance(self.details, dict) and self.details.get("error"):
            raise McpError(f"{self.label}: {self.text}", name=self.name, call_id=self.id,
                           details=self.details, status=str(self.details.get("error")))
        return ToolResult(self.text, details=self.details, name=self.label, call_id=self.id, ok=True,
                          truncated=self.truncated, elapsed_seconds=self.elapsed_seconds)

    async def result(self, wait: float | None = None) -> ToolResult:
        """Wait for the call to end and return its ToolResult (raises ToolError when it failed or was cancelled).

        With ``wait`` seconds, raises TimeoutError if it is still running then."""
        if wait is not None and (isinstance(wait, bool) or not isinstance(wait, (int, float)) or wait < 0):
            raise ValueError("wait must be a non-negative number of seconds")
        payload: dict[str, Any] = {"id": self.id}
        if wait is not None:
            payload["wait"] = wait
        self._update(await self._bridge.request("tools.result", payload))
        if self.running:
            raise TimeoutError(f"{self.label} ({self.id}) is still running after {wait:g} s")
        return self._as_result()

    async def cancel(self) -> "ToolCall":
        """Stop the call and return this handle, refreshed."""
        self._update(await self._bridge.request("tools.cancel", {"id": self.id}))
        return self

    async def refresh(self) -> "ToolCall":
        self._update(await self._bridge.request("tools.get", {"id": self.id}))
        return self

    def __repr__(self) -> str:
        if self.running:
            return (f"ToolCall(id={self.id!r}, {self.label}, running {self.elapsed_seconds:g}s: completion arrives "
                    "as a <runtime_event>; await call.result() to wait)")
        return f"ToolCall(id={self.id!r}, {self.label}, status={self.status!r}, {self.elapsed_seconds:g}s)"


def _yield_payload(yield_after: Any) -> tuple[dict[str, Any], str, float | None]:
    """The host payload fields for a yield_after argument, the return mode, and the automatic wait."""
    if yield_after is _DEFAULT:
        auto = _default_yield_after()
        return ({} if auto is None else {"yield_after": auto}), "auto", auto
    if yield_after is None:
        return {}, "block", None
    if isinstance(yield_after, bool) or not isinstance(yield_after, (int, float)) or yield_after < 0:
        raise ValueError("yield_after must be a non-negative number of seconds, or None")
    return {"yield_after": min(float(yield_after), _MAX_WAIT_SECONDS), "detach": True}, "handle", None


class _BoundTool:
    def __init__(self, tools: "Tools", name: str) -> None:
        self._tools = tools
        self._name = name

    async def __call__(self, params: dict[str, Any] | None = None, /, *, yield_after: Any = _DEFAULT, **kwargs: Any) -> Any:
        return await self._tools.call(self._name, params, yield_after=yield_after, **kwargs)

    def __repr__(self) -> str:
        return f"<extension tool {self._name}: await tools.describe({self._name!r}) for its parameters>"


class Tools:
    """Tools registered by Pi extensions, called from Python (see the module docstring)."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def list(self) -> list[dict[str, Any]]:
        """Every extension tool: [{name, label, description}]."""
        return await self._bridge.request("tools.list", {})

    async def describe(self, name: str) -> dict[str, Any]:
        """One tool's name, full description and JSON-schema parameters."""
        return await self._bridge.request("tools.describe", {"name": name})

    async def call(self, name: str, params: dict[str, Any] | None = None, /, *, yield_after: Any = _DEFAULT,
                   **kwargs: Any) -> Any:
        """Call extension tool ``name`` with ``params`` (a dict) and/or keyword arguments.

        Returns a ToolResult (the text; .details). Left out, ``yield_after`` waits up to ULTRON_TOOL_YIELD_AFTER
        seconds (default 30), then a call still running continues in the host (.running, .call) and its end
        arrives as a runtime event. None waits however long it takes; a number returns a ToolCall handle after at
        most that long. Raises ToolError when the tool fails.
        """
        if not isinstance(name, str) or not name:
            raise ValueError("tool name must be a non-empty string")
        if params is not None and not isinstance(params, dict):
            raise TypeError("params must be a dict")
        arguments = {**(params or {}), **kwargs}
        return await self._invoke(name, arguments, yield_after)

    async def _invoke(self, name: str, arguments: dict[str, Any], yield_after: Any) -> Any:
        fields, mode, auto = _yield_payload(yield_after)
        data = await self._bridge.request("tools.call", {"name": name, "params": arguments, **fields})
        call = ToolCall(self._bridge, data)
        if mode == "handle":
            return call
        return call._as_result(waited=auto)

    async def calls(self) -> list[dict[str, Any]]:
        """Recent calls (running first is not guaranteed; newest first), without their results."""
        return await self._bridge.request("tools.calls", {})

    async def get(self, call_id: str) -> ToolCall:
        """A handle for call ``call_id`` (from a runtime event or ``tools.calls()``)."""
        return ToolCall(self._bridge, await self._bridge.request("tools.get", {"id": call_id}))

    async def result(self, call_id: str, wait: float | None = None) -> ToolResult:
        """The ToolResult of call ``call_id``, waiting for it to end."""
        return await (await self.get(call_id)).result(wait)

    def __getattr__(self, name: str) -> _BoundTool:
        if name.startswith("_"):
            raise AttributeError(name)
        return _BoundTool(self, name)

    def __repr__(self) -> str:
        return ("tools (extension tools: await tools.list(), tools.describe(name), tools.call(name, params), "
                "tools.<name>(**params), tools.calls(), tools.get(id))")


def _identifier(name: str) -> str:
    return re.sub(r"\W", "_", name)


class _McpServer:
    """``mcp.<server>``: the server's tools by attribute (``await mcp.exa_agent.exa_agent_run(query=...)``)."""

    def __init__(self, mcp: "Mcp", attr: str) -> None:
        self._mcp = mcp
        self._attr = attr

    async def _name(self) -> str:
        return await self._mcp._server_named(self._attr)

    async def tools(self) -> list[str]:
        return await self._mcp.tools(await self._name())

    async def describe(self, tool: str) -> dict[str, Any]:
        return await self._mcp.describe(tool, server=await self._name())

    def __getattr__(self, tool: str) -> Any:
        if tool.startswith("_"):
            raise AttributeError(tool)

        async def call(args: dict[str, Any] | str | None = None, /, *, yield_after: Any = _DEFAULT, **kwargs: Any) -> Any:
            return await self._mcp.call(tool, args, server=await self._name(), yield_after=yield_after, **kwargs)

        call.__name__ = tool
        call.__doc__ = f"MCP tool {tool} on server mcp.{self._attr}; keyword arguments are the tool's arguments."
        return call

    def __repr__(self) -> str:
        return f"<MCP server mcp.{self._attr}: await mcp.{self._attr}.tools(), mcp.{self._attr}.<tool>(**args)>"


class Mcp:
    """MCP servers through Ultron's ``mcp`` gateway tool (see the module docstring)."""

    def __init__(self, tools: Tools, gateway: str = "mcp") -> None:
        self._tools = tools
        self._gateway_name = gateway
        self._server_names: dict[str, str] = {}

    async def _gateway(self, **params: Any) -> ToolResult:
        return await self._tools._invoke(self._gateway_name, params, None)

    async def servers(self) -> list[dict[str, Any]]:
        """Configured servers: [{name, status, toolCount, ...}]."""
        result = await self._gateway()
        details = result.details if isinstance(result.details, dict) else {}
        return list(details.get("servers") or [])

    async def tools(self, server: str | None = None) -> list[str]:
        """Tool names of ``server`` (connecting a lazy server when needed), or of every enabled server."""
        if server is None:
            names: list[str] = []
            for item in await self.servers():
                if item.get("disabled"):
                    continue
                try:
                    names.extend(await self.tools(item["name"]))
                except McpError:
                    continue
            return names
        try:
            result = await self._gateway(server=server)
        except McpError as error:
            if not isinstance(error.details, dict) or error.details.get("error") != "not_connected":
                raise
            await self.connect(server)
            result = await self._gateway(server=server)
        details = result.details if isinstance(result.details, dict) else {}
        return list(details.get("tools") or [])

    async def describe(self, tool: str, server: str | None = None) -> dict[str, Any]:
        """{name, server, description, parameters (JSON schema), text} of one MCP tool."""
        params: dict[str, Any] = {"describe": tool}
        if server is not None:
            params["server"] = server
        result = await self._gateway(**params)
        details = result.details if isinstance(result.details, dict) else {}
        meta = details.get("tool") if isinstance(details.get("tool"), dict) else {}
        return {
            "name": meta.get("name", tool),
            "server": details.get("server", server),
            "description": meta.get("description", ""),
            "parameters": meta.get("inputSchema"),
            "text": str(result),
        }

    async def search(self, query: str, server: str | None = None, limit: int | None = None) -> list[dict[str, Any]]:
        """Tools matching ``query``: [{server, tool, score}]."""
        params: dict[str, Any] = {"search": query, "includeSchemas": False}
        if server is not None:
            params["server"] = server
        if limit is not None:
            params["limit"] = limit
        result = await self._gateway(**params)
        details = result.details if isinstance(result.details, dict) else {}
        return list(details.get("matches") or [])

    async def connect(self, server: str) -> str:
        """Connect a (lazy) server and refresh its tool list."""
        return str(await self._gateway(connect=server))

    async def instructions(self, server: str) -> str:
        """A server's usage instructions."""
        return str(await self._gateway(instructions=server))

    async def resources(self, server: str | None = None) -> dict[str, Any]:
        """{resources, resourceTemplates} of ``server`` (or of every enabled server), each tagged with its server."""
        result = await self._gateway(resources=server if server is not None else True)
        details = result.details if isinstance(result.details, dict) else {}
        return {"resources": list(details.get("resources") or []),
                "resourceTemplates": list(details.get("resourceTemplates") or [])}

    async def read(self, server: str, uri: str) -> ToolResult:
        """Read resource ``uri`` of ``server``: a ToolResult with its text (binary resources are saved to a file)."""
        return await self._gateway(server=server, read=uri)

    async def call(self, tool: str, args: dict[str, Any] | str | None = None, /, *, server: str | None = None,
                   yield_after: Any = _DEFAULT, **kwargs: Any) -> Any:
        """Call MCP tool ``tool``; keyword arguments (and/or an ``args`` dict or JSON string) are its arguments.

        Returns a ToolResult (``.json()`` parses JSON text; ``.details`` has the gateway's details). Raises McpError
        when the gateway reports an error. ``yield_after`` works as in ``tools.call``.
        """
        if isinstance(args, str):
            args = json.loads(args) if args.strip() else {}
        if args is not None and not isinstance(args, dict):
            raise TypeError("args must be a dict or a JSON object string")
        params: dict[str, Any] = {"tool": tool, "args": {**(args or {}), **kwargs}}
        if server is not None:
            params["server"] = server
        return await self._tools._invoke(self._gateway_name, params, yield_after)

    async def _server_named(self, attr: str) -> str:
        if attr in self._server_names:
            return self._server_names[attr]
        names = [item.get("name") for item in await self.servers() if isinstance(item.get("name"), str)]
        for name in names:
            self._server_names[_identifier(name)] = name
            self._server_names[name] = name
        if attr not in self._server_names:
            raise AttributeError(f"no MCP server mcp.{attr}; servers: {', '.join(names) or '(none)'}")
        return self._server_names[attr]

    def __getattr__(self, attr: str) -> _McpServer:
        if attr.startswith("_"):
            raise AttributeError(attr)
        return _McpServer(self, attr)

    def __repr__(self) -> str:
        return ("mcp (MCP servers: await mcp.servers(), mcp.tools(server), mcp.describe(tool), mcp.search(q), "
                "mcp.call(tool, **args), mcp.<server>.<tool>(**args))")
