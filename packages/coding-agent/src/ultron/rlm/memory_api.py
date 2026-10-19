class Memory:
    def __init__(self, bridge): self._bridge = bridge
    # Memory is used on purpose: nothing recalls or keeps on its own. A write holding a secret is refused.
    async def prepare(self, query, *, scope='session', task_id='rlm'):
        return await self._bridge.request('memory.prepare', {'query': query, 'scope': scope, 'taskId': task_id})
    async def why(self, task_id): return await self._bridge.request('memory.why', {'taskId': task_id})
    async def propose(self, text, evidence, *, scope='session', type=None, key=None, situation=None, path_globs=None):
        return await self._bridge.request('memory.propose', _propose(text, evidence, scope, type, key, situation, path_globs))
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


def _propose(text, evidence, scope, type, key, situation, path_globs):
    # Typed records (Rust host only; sent only when given): type is preference, fact, procedure, pitfall or
    # reference; situation says in one line when it applies; key (kebab-case) replaces the record with that key;
    # path_globs (e.g. ['src/**/*.rs']) make a matching file change point at the record.
    payload = {'text': text, 'evidence': evidence, 'scope': scope}
    for name, value in (('type', type), ('key', key), ('situation', situation), ('pathGlobs', path_globs)):
        if value is not None:
            payload[name] = list(value) if name == 'pathGlobs' else value
    return payload
