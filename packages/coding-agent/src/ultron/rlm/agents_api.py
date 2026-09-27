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
    """Typed agents run by the Ultron host.

    `await agents.list()` shows definitions (for example "rlm-child@1" with input `{"prompt": ...}`);
    `await agents.invoke(definition, input)` runs one and returns `{"status": ..., "value": ...}`;
    `t = await agents.spawn(definition, input)` starts one in the background (its completion arrives as a
    `task_done` event) and `await t.result()` collects it. `agents.tasks()`, `agents.inspect(id)`,
    `agents.result(id)`, `agents.cancel(id)` and `agents.status()` manage tasks.
    """

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
        """Tasks, definitions and usage of this root. `spend` is the root tree's model spend: turns, tokens and
        costUsd of every response of the root and each lane it admitted, beside its turn, token and cost limits."""
        return await self._bridge.request('agents.status')


class Workflows:
    """`await workflows.run(nodes)` validates an agent graph and runs it (see `help(workflows.run)`)."""

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
