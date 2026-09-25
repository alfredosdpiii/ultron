"""Frozen release gates and recorded baseline-versus-candidate comparisons."""


class ReleaseGates:
    def __init__(self, bridge):
        self._bridge = bridge

    async def define(self, gate_id, checks, fixture_hash):
        return await self._bridge.request('gates.define', {'id': gate_id, 'checks': checks, 'fixture_hash': fixture_hash})

    async def compare(self, gate_id, baseline, candidate):
        return await self._bridge.request('gates.compare', {'gate_id': gate_id, 'baseline': baseline, 'candidate': candidate})

    async def history(self, gate_id):
        return await self._bridge.request('gates.history', {'gate_id': gate_id})

    async def list(self):
        return await self._bridge.request('gates.list', {})
