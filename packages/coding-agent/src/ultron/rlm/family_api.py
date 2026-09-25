"""Family messaging client. The host derives the sender from the calling lane and enforces scope."""
from typing import Any


class AgentMessages:
    def __init__(self, bridge):
        self._bridge = bridge

    async def send(
        self,
        message: str,
        *,
        receiver_role: str = "parent",
        receiver_name: str | None = None,
        receiver_id: str | None = None,
        key: str | None = None,
        ttl_ms: int | None = None,
        steer: bool = False,
    ) -> dict[str, Any]:
        if not isinstance(message, str) or not message.strip():
            raise ValueError("agent_message.send message must be non-empty")
        if receiver_role not in {"parent", "child"}:
            raise ValueError("receiver_role must be parent or child")
        payload: dict[str, Any] = {"message": message, "receiver_role": receiver_role}
        if receiver_name is not None: payload["receiver_name"] = receiver_name
        if receiver_id is not None: payload["receiver_id"] = receiver_id
        if key is not None: payload["key"] = key
        if ttl_ms is not None: payload["ttl_ms"] = ttl_ms
        if steer: payload["steer"] = True
        result = await self._bridge.request("agent_message.send", payload)
        return result if isinstance(result, dict) else {"result": result}

    async def receive(self, limit: int | None = None) -> dict[str, Any]:
        payload: dict[str, Any] = {} if limit is None else {"limit": limit}
        return await self._bridge.request("agent_message.receive", payload)

    async def list(self) -> dict[str, Any]:
        return await self._bridge.request("agent_message.list", {})
