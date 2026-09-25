class Memory:
    def __init__(self, bridge): self._bridge = bridge
    # Calls from agent code are deliberate: explicit=True skips Jev's relevance gate (sensitive writes are still refused).
    async def prepare(self, query, *, scope='session', task_id='rlm', explicit=True):
        return await self._bridge.request('memory.prepare', {'query': query, 'scope': scope, 'taskId': task_id, 'explicit': explicit})
    async def why(self, task_id): return await self._bridge.request('memory.why', {'taskId': task_id})
    async def propose(self, text, evidence, *, scope='session', explicit=True):
        return await self._bridge.request('memory.propose', {'text': text, 'evidence': evidence, 'scope': scope, 'explicit': explicit})
    async def correct(self, memory_id, text, evidence): return await self._bridge.request('memory.correct', {'id': memory_id, 'text': text, 'evidence': evidence})
    async def forget(self, memory_id): return await self._bridge.request('memory.forget', {'id': memory_id})
    async def get(self, memory_id): return await self._bridge.request('memory.get', {'id': memory_id})
    async def list(self): return await self._bridge.request('memory.list')

class Refinements:
    def __init__(self, bridge): self._bridge = bridge
    async def propose(self, kind, target, base_version, content, evidence, scope='session'):
        return await self._bridge.request('refinements.propose', {'kind':kind,'target':target,'baseVersion':base_version,'content':content,'evidence':evidence,'scope':scope})
    async def list(self): return await self._bridge.request('refinements.list')
    async def activate(self, id): return await self._bridge.request('refinements.activate', {'id':id})
    async def rollback(self, id): return await self._bridge.request('refinements.rollback', {'id':id})
    async def reject(self, id): return await self._bridge.request('refinements.reject', {'id':id})
