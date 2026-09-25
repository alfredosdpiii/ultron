"""Schedule and goal RPC clients. The host owns timing, budgets, and verification."""


def _compact(payload):
    return {key: value for key, value in payload.items() if value is not None}


class Schedules:
    def __init__(self, bridge):
        self._bridge = bridge

    async def create(self, definition, input, every_ms, *, start_at=None, max_runs=None, goal_id=None, model=None):
        return await self._bridge.request('schedules.create', _compact({
            'definition': definition, 'input': input, 'every_ms': every_ms, 'start_at': start_at,
            'max_runs': max_runs, 'goal_id': goal_id, 'model': model,
        }))

    async def list(self):
        return await self._bridge.request('schedules.list')

    async def pause(self, id):
        return await self._bridge.request('schedules.pause', {'id': id})

    async def resume(self, id):
        return await self._bridge.request('schedules.resume', {'id': id})

    async def delete(self, id):
        return await self._bridge.request('schedules.delete', {'id': id})

    async def tick(self):
        return await self._bridge.request('schedules.tick')


class Goals:
    def __init__(self, bridge):
        self._bridge = bridge

    async def create(self, title, required_checks, *, max_tasks=None):
        return await self._bridge.request('goals.create', _compact({
            'title': title, 'required_checks': list(required_checks), 'max_tasks': max_tasks,
        }))

    async def list(self):
        return await self._bridge.request('goals.list')

    async def get(self, id):
        return await self._bridge.request('goals.get', {'id': id})

    async def pause(self, id):
        return await self._bridge.request('goals.pause', {'id': id})

    async def resume(self, id):
        return await self._bridge.request('goals.resume', {'id': id})

    async def attach(self, id, task_id):
        return await self._bridge.request('goals.attach', {'id': id, 'task_id': task_id})

    async def verify(self, id, *, input=None):
        payload = {'id': id}
        if input is not None:
            payload['input'] = input
        return await self._bridge.request('goals.verify', payload)
