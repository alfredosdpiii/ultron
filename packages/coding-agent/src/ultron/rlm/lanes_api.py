"""REPL lanes, running cells, never-waiting results and kernel recovery: the `rlm` methods for the Rust host's lane API.

A root may run several lanes, each a kernel of its own; a cell that outlives its tool call or waits on the host
does not hold the others. Hosts without lanes (the TypeScript host) answer these requests with an error.
"""

from __future__ import annotations

from typing import Any


def child_selectors(selectors: Any) -> list[Any]:
    """SpawnHandles or ids (one or a list; none: all of yours) as the ids the host selects children by."""
    if isinstance(selectors, str) or hasattr(selectors, "rlm_child_id"):
        selectors = [selectors]
    return [getattr(s, "rlm_child_id", s) for s in (selectors or [])]


class LaneControl:
    """REPL lanes: each lane is a kernel of its own (its own variables) that runs one cell at a time, so a cell
    that outlives its tool call, or waits on the host, does not hold the others.

    `await rlm.lane.open("side")` opens a lane (idempotent; its kernel starts with its first cell; a lane limit
    applies); `await rlm.lane.close("side")` closes an idle one and shuts its kernel down. `await rlm.lanes()`
    lists them; `await rlm.cells()` lists running cells and `await rlm.cell.cancel(id)` cancels one."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def open(self, name: str) -> dict[str, Any]:
        """Open lane `name` (idempotent): `{"opened": True}`."""
        return await self._bridge.request("rlm.lane.open", {"lane": name})

    async def close(self, name: str) -> dict[str, Any]:
        """Close idle lane `name` and shut its kernel down (its variables are gone): `{"closed": True}`. A lane
        running a cell refuses: cancel the cell first."""
        return await self._bridge.request("rlm.lane.close", {"lane": name})


class CellControl:
    """Running cells (`await rlm.cells()` lists them)."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def cancel(self, cell_id: str) -> dict[str, Any]:
        """Cancel another lane's running cell and wait for its end: `{"cancelled": True, "status": ...}`. Its
        host waits end first (the cell raises and its kernel keeps its variables); a cell still running after a
        grace period has its kernel killed (that lane's variables are lost). A cell cannot cancel itself."""
        return await self._bridge.request("rlm.cell.cancel", {"cell": cell_id})


class LaneMethods:
    """`rlm.results`, `rlm.lanes`, `rlm.cells`, `rlm.cancel` and `rlm.recovery` (see [install])."""

    _bridge: Any

    async def results(self, selectors: Any = None) -> dict[str, Any]:
        """Never waits: `{"results": [...], "running": [ids]}`, the results (as `rlm.collect`) of the selected
        subagents that ended (SpawnHandles or ids; all of yours when empty) and the ids still running."""
        result = await self._bridge.request("rlm.results", {"selectors": child_selectors(selectors)})
        if not isinstance(result, dict):
            return {"results": [], "running": []}
        return {"results": result.get("results", []), "running": result.get("running", [])}

    async def lanes(self) -> list[dict[str, Any]]:
        """Open REPL lanes: `[{"lane": name, "cell": running cell id or None, "pid": kernel pid or None}]`
        (`help(rlm.lane)`)."""
        result = await self._bridge.request("rlm.lanes", {})
        return result.get("lanes", []) if isinstance(result, dict) else []

    async def cells(self) -> list[dict[str, Any]]:
        """Cells still running in any lane, this one included: `[{"cell": id, "lane": name, "parked": waits on
        the host, "detached": its end arrives as an event}]` (`help(rlm.cell)`)."""
        result = await self._bridge.request("rlm.cells", {})
        return result.get("cells", []) if isinstance(result, dict) else []

    async def cancel(self, cell_id: str) -> dict[str, Any]:
        """Cancel a running cell of any lane but this one (an id from `await rlm.cells()`, or the one a side-lane
        note names) and wait for its end: `{"cancelled": True, "status": ...}`. Its host work stops first; a cell
        still running after a grace period has its kernel killed with every process it started."""
        return await self._bridge.request("rlm.cancel", {"cell": cell_id})

    async def recovery(self) -> dict[str, Any] | None:
        """This lane's latest kernel recovery, or None: after the kernel died and restarted, `{"reason",
        "snapshotPath", "restored": [names], "lost": [{"name", "reason"}], "interruptedCell", "unsavedCells",
        "restoreError"?}`. The first cell after a restart also prints it at the top of its stderr."""
        return await self._bridge.request("rlm.recovery", {})


def install(namespace_class: type) -> None:
    """Give the `rlm` namespace class these methods (so `help(rlm)` lists them) and `rlm.lane` / `rlm.cell`."""
    for name in ("results", "lanes", "cells", "cancel", "recovery"):
        setattr(namespace_class, name, LaneMethods.__dict__[name])
    namespace_class.lane = property(lambda self: LaneControl(self._bridge), doc=LaneControl.__doc__)
    namespace_class.cell = property(lambda self: CellControl(self._bridge), doc=CellControl.__doc__)
