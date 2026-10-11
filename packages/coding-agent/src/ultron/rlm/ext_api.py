"""Extension functions as kernel-API functions: ``await ext.<extension>.<function>(**kwargs)``.

Extensions are separate processes the host runs; the functions they register are never model tools. The host
lists them in ``ULTRON_EXT_FUNCTIONS`` (JSON ``[{extension, name, description, signature}]``) when it starts the
kernel (`signature` may be replaced by the JSON-schema `parameters`), and every call goes through the ``ext.call`` host request. ``ext`` is absent when no extension registered
a function.
"""

from __future__ import annotations

import json
import os
import re
from typing import Any

from tools_api import ToolError, ToolResult

ENV = "ULTRON_EXT_FUNCTIONS"


def _attr(name: Any) -> str | None:
    """The Python attribute for an extension or function name (`my-ext` -> `my_ext`)."""
    if not isinstance(name, str):
        return None
    attr = re.sub(r"\W", "_", name)
    return attr if attr.isidentifier() and not attr.startswith("_") else None


def _signature(parameters: Any) -> str:
    """`name: type` per top-level schema property, in order (as the Rust host's runtime-guide section)."""
    properties = parameters.get("properties") if isinstance(parameters, dict) else None
    if not isinstance(properties, dict):
        return "**kwargs"
    return ", ".join(
        f"{key}: {schema['type']}" if isinstance(schema, dict) and isinstance(schema.get("type"), str) else key
        for key, schema in properties.items()
    )


def _listing(raw: Any) -> list[dict[str, str]]:
    """Entries `{extension, name, description, signature | parameters}`, keeping those with usable names."""
    if not isinstance(raw, list):
        return []
    out = []
    for item in raw:
        if not isinstance(item, dict) or _attr(item.get("extension")) is None or _attr(item.get("name")) is None:
            continue
        signature = item.get("signature")
        out.append({
            "extension": item["extension"],
            "name": item["name"],
            "description": str(item.get("description") or ""),
            "signature": signature if isinstance(signature, str) else _signature(item.get("parameters")),
        })
    return out


def _line(item: dict[str, str]) -> str:
    line = f"ext.{_attr(item['extension'])}.{_attr(item['name'])}({item['signature']})"
    return f"{line} - {item['description']}" if item["description"] else line


def _result(label: str, reply: Any) -> ToolResult:
    if not isinstance(reply, dict):
        return ToolResult("" if reply is None else str(reply), name=label)
    content = reply.get("content")
    texts = [part.get("text", "") for part in content if isinstance(part, dict) and part.get("type") == "text"] \
        if isinstance(content, list) else []
    text = "\n".join(t for t in texts if isinstance(t, str))
    details = reply.get("details")
    if reply.get("isError"):
        raise ToolError(f"{label} failed: {text or 'error'}", name=label, details=details, status="failed")
    return ToolResult(text, details=details, name=label)


def _function(bridge: Any, item: dict[str, str]) -> Any:
    extension, name = item["extension"], item["name"]
    label = f"ext.{extension}.{name}"

    async def call(**kwargs: Any) -> ToolResult:
        reply = await bridge.request("ext.call", {"extension": extension, "function": name, "arguments": kwargs})
        return _result(label, reply)

    call.__name__ = call.__qualname__ = _attr(name) or name
    call.__doc__ = _line(item) + "\n\nKeyword arguments only. Returns a ToolResult (the text; .details, .json())."
    return call


class Extension:
    """One extension's functions."""

    def __init__(self, name: str, functions: dict[str, Any]) -> None:
        self._name = name
        self._functions = functions
        for key, fn in functions.items():
            setattr(self, key, fn)

    def __getattr__(self, attr: str) -> Any:
        if attr.startswith("_"):
            raise AttributeError(attr)
        known = ", ".join(sorted(self._functions)) or "none"
        raise AttributeError(f"extension {self._name} has no function {attr} (it has: {known}); help(ext) lists them")

    def __dir__(self) -> list[str]:
        return sorted(self._functions)

    def __repr__(self) -> str:
        return f"<ext.{self._name}: {', '.join(sorted(self._functions))}>"


class Extensions:
    """Extension functions: `await ext.<extension>.<function>(**kwargs)` returns a ToolResult (the text, with
    .details); an error result raises ToolError. `await ext.refresh()` reloads this listing."""

    def __init__(self, bridge: Any, listing: list[dict[str, str]]) -> None:
        self._bridge = bridge
        self._install(listing)

    def _install(self, listing: list[dict[str, str]]) -> None:
        for name in list(getattr(self, "_extensions", {})):
            delattr(self, name)
        groups: dict[str, dict[str, Any]] = {}
        for item in listing:
            groups.setdefault(_attr(item["extension"]), {})[_attr(item["name"])] = _function(self._bridge, item)
        self._extensions = {name: Extension(name, fns) for name, fns in groups.items()}
        for name, extension in self._extensions.items():
            setattr(self, name, extension)
        doc = Extensions.__doc__ + "\n\n" + ("\n".join(_line(item) for item in listing) or "No extension functions.")
        # help(ext) shows the class docstring: give this instance a class whose docstring lists the functions.
        self.__class__ = type("ext", (Extensions,), {"__doc__": doc})

    async def refresh(self) -> list[str]:
        """Reload the function listing from the host (after an extension registered new functions)."""
        listing = _listing(await self._bridge.request("ext.list", {}))
        self._install(listing)
        return [_line(item) for item in listing]

    def __getattr__(self, attr: str) -> Any:
        if attr.startswith("_"):
            raise AttributeError(attr)
        known = ", ".join(sorted(self._extensions)) or "none"
        raise AttributeError(f"no extension {attr} (loaded: {known}); help(ext) lists their functions")

    def __dir__(self) -> list[str]:
        return sorted(self._extensions) + ["refresh"]

    def __repr__(self) -> str:
        return f"<ext: {', '.join(sorted(self._extensions)) or 'no extensions'}>"


def install(namespace: dict[str, Any], bridge: Any) -> None:
    """Bind `ext` when the host listed extension functions; leave it absent otherwise."""
    try:
        listing = _listing(json.loads(os.environ.get(ENV, "") or "[]"))
    except json.JSONDecodeError:
        listing = []
    if listing:
        namespace["ext"] = Extensions(bridge, listing)
