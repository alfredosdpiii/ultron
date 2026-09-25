"""Typed agent RPC client. The existing Pi host owns execution; no second loop."""
from dataclasses import dataclass


@dataclass
class TaskHandle:
    id: str
    _bridge: object

    async def result(self):
        return await self._bridge.request('agents.result', {'id': self.id})

    async def inspect(self):
        return await self._bridge.request('agents.inspect', {'id': self.id})

    async def cancel(self):
        return await self._bridge.request('agents.cancel', {'id': self.id})


class Agents:
    def __init__(self, bridge):
        self._bridge = bridge

    async def list(self):
        return await self._bridge.request('agents.list')

    async def register(self, definition):
        return await self._bridge.request('agents.register', {'definition': definition})

    async def spawn(self, definition, input, *, model=None, key=None):
        result = await self._bridge.request('agents.spawn', {'definition': definition, 'input': input, 'model': model, 'key': key})
        return TaskHandle(result['id'], self._bridge)

    async def invoke(self, definition, input, *, model=None, key=None):
        return await self._bridge.request('agents.invoke', {'definition': definition, 'input': input, 'model': model, 'key': key})

    async def tasks(self):
        return await self._bridge.request('agents.tasks')

    async def result(self, task_id):
        return await self._bridge.request('agents.result', {'id': task_id})

    async def inspect(self, task_id):
        return await self._bridge.request('agents.inspect', {'id': task_id})

    async def cancel(self, task_id):
        return await self._bridge.request('agents.cancel', {'id': task_id})

    async def status(self):
        return await self._bridge.request('agents.status')


class Workflows:
    def __init__(self, bridge):
        self._bridge = bridge

    async def run(self, nodes):
        return await self._bridge.request('workflows.run', {'nodes': nodes})
