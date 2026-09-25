class Instances:
    """Retained agent instances: continue a completed RLM task on its own lane."""

    def __init__(self, bridge):
        self._bridge = bridge

    async def retain(self, task_id):
        return await self._bridge.request('instances.retain', {'task_id': task_id})

    async def invoke(self, instance_id, input, *, key=None):
        payload = {'id': instance_id, 'input': input}
        if key is not None:
            payload['key'] = key
        return await self._bridge.request('instances.invoke', payload)

    async def close(self, instance_id):
        return await self._bridge.request('instances.close', {'id': instance_id})

    async def get(self, instance_id):
        return await self._bridge.request('instances.get', {'id': instance_id})

    async def list(self):
        return await self._bridge.request('instances.list', {})
