"""Skill catalog client. Selection is explainable and version-pinned; skills never grant capabilities."""


class Skills:
    def __init__(self, bridge):
        self._bridge = bridge

    async def refresh(self):
        return await self._bridge.request('skills.refresh', {})

    async def list(self):
        return await self._bridge.request('skills.list', {})

    async def select(self, query, *, limit=None):
        payload = {'query': query}
        if limit is not None:
            payload['limit'] = limit
        return await self._bridge.request('skills.select', payload)

    async def load(self, name, *, version=None):
        payload = {'name': name}
        if version is not None:
            payload['version'] = version
        return await self._bridge.request('skills.load', payload)

    async def why(self, decision_id):
        return await self._bridge.request('skills.why', {'decision_id': decision_id})

    async def invoke(self, name, version, input):
        return await self._bridge.request('skills.invoke', {'name': name, 'version': version, 'input': input})
