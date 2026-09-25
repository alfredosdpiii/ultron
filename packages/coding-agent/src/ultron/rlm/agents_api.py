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

    async def run(self, nodes, key=None):
        """Validate the whole graph before any effect, then run it; returns {node_id: outcome}.

        Each node: id, definition, and input or inputFrom (one dependency, or a list for
        fan-in as {dep_id: value}); optional dependsOn, when={node, field?, equals}, key,
        model, timeout_ms, join and revise.

        join: "all" (default) runs only if every dependency succeeded, else the node is
        skipped. "any" (two or more dependencies; inputFrom must be a list) waits for every
        dependency to finish, runs if at least one succeeded, and is skipped only if none
        did; its fan-in holds only the dependencies that succeeded.

        revise={from: reviewer_id, until: {field?, equals}, max_rounds: 1..10} makes a
        bounded revision loop: the node runs, then the reviewer (which must depend on it),
        and while the review does not match `until` the node runs again with input
        {input, previous, review, round}. The node's outcome carries
        revision={outcome: converged|exhausted|failed, rounds, max_rounds} and every
        round's work and review; an exhausted loop has status "exhausted", so all-joins
        downstream skip. Every round is its own task and budget admission.

        key: optional workflow key; nodes default to key "<key>:<node>" and loop rounds to
        "<node key>:round-<n>", so a rerun reuses completed tasks and rounds.
        """
        payload = {'nodes': nodes}
        if key is not None:
            payload['key'] = key
        return await self._bridge.request('workflows.run', payload)
