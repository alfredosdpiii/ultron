"""`schedule`: prompts this session sends itself later (the Rust host; the TypeScript host answers with an error).

The host keeps each schedule in the session and an Ultron server fires it: the prompt arrives as a new turn of this
session (after a run in progress, never interrupting it). Missed fires while no server ran collapse into one.
"""

from __future__ import annotations

import datetime as _dt
from typing import Any


def _number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _time(value: Any, what: str, prefix: str) -> dict[str, Any]:
    """A time as the host takes it: `{prefix}_ms` (epoch ms) or `{prefix}_in_ms` (from now)."""
    if _number(value):
        if value <= 0:
            raise ValueError(f"schedule {what} in seconds must be positive")
        return {f"{prefix}_in_ms": round(value * 1000)}
    if isinstance(value, _dt.timedelta):
        return _time(value.total_seconds(), what, prefix)
    if isinstance(value, str):
        text = value.strip()
        if text.endswith("Z"):
            text = text[:-1] + "+00:00"
        try:
            value = _dt.datetime.fromisoformat(text)
        except ValueError:
            raise ValueError(f"schedule {what} must be an ISO time such as '2026-10-20T09:00' or seconds") from None
    if isinstance(value, _dt.datetime):
        if value.tzinfo is None:
            value = value.astimezone()
        return {f"{prefix}_ms": round(value.timestamp() * 1000)}
    raise ValueError(f"schedule {what} must be an ISO time, a datetime or a number of seconds from now")


def _interval(value: Any) -> Any:
    if isinstance(value, _dt.timedelta):
        return value.total_seconds()
    if _number(value) or isinstance(value, str):
        return value
    raise ValueError("schedule interval must be like '90s', '15m', '1h', '1d' or a number of seconds")


def _prompt(prompt: Any) -> str:
    if not isinstance(prompt, str) or not prompt.strip():
        raise ValueError("schedule prompt must be a non-empty string")
    return prompt


class Schedule:
    """Prompts this session sends itself later, each starting a turn of its own (one model run per fire).

    `id = await schedule.every("15m", "check CI on main and fix what broke", name="ci")` repeats (interval "90s",
    "15m", "1h", "1d", "1h30m" or seconds; at least 60 s; `until=` a time, `max_runs=` a count);
    `await schedule.at("2026-10-20T09:00", "summarize overnight failures")` runs once (an ISO time, a datetime or
    seconds from now). `await schedule.list()` lists them (name, next, runs), `await schedule.cancel(name_or_id)`
    stops one. Each fired prompt reaches you as a message of the session; the user sees every schedule in
    /schedules. Only the root agent schedules."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def every(self, interval: Any, prompt: str, *, name: str | None = None, until: Any = None,
                    max_runs: int | None = None) -> str:
        """Send `prompt` every `interval`, the first time one interval from now; returns the schedule id."""
        payload: dict[str, Any] = {"interval": _interval(interval), "prompt": _prompt(prompt)}
        if name is not None:
            payload["name"] = str(name)
        if until is not None:
            payload.update(_time(until, "until", "until"))
        if max_runs is not None:
            if isinstance(max_runs, bool) or not isinstance(max_runs, int) or max_runs < 1:
                raise ValueError("schedule max_runs must be a positive integer")
            payload["max_runs"] = max_runs
        return (await self._bridge.request("schedule.every", payload))["id"]

    async def at(self, when: Any, prompt: str, name: str | None = None) -> str:
        """Send `prompt` once at `when` (ISO time, datetime, or seconds from now); returns the schedule id."""
        payload: dict[str, Any] = {"prompt": _prompt(prompt), **_time(when, "time", "when")}
        if name is not None:
            payload["name"] = str(name)
        return (await self._bridge.request("schedule.at", payload))["id"]

    async def list(self) -> list[dict[str, Any]]:
        """This session's schedules: `{id, name, prompt, every, next, runs, maxRuns, state, ...}`."""
        return await self._bridge.request("schedule.list", {})

    async def cancel(self, name_or_id: str) -> dict[str, Any]:
        """Stop the active schedule with this id or name; returns it (state "cancelled")."""
        if not isinstance(name_or_id, str) or not name_or_id.strip():
            raise ValueError("schedule.cancel needs a schedule id or name")
        return await self._bridge.request("schedule.cancel", {"id": name_or_id.strip()})

    def __repr__(self) -> str:
        return "<schedule: every(interval, prompt, name=, until=, max_runs=), at(when, prompt), list(), cancel(id)>"
