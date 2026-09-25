"""Scoped approvals bound to scope, revision, policy, and action. The host derives the owner from the lane.

Grants are advisory unless the host enables enforcement; check results report `enforced`.
"""


class Grants:
    def __init__(self, bridge):
        self._bridge = bridge

    async def issue(self, scope, revision, policy, action, ttl_ms):
        return await self._bridge.request('grants.issue', {'scope': scope, 'revision': revision, 'policy': policy, 'action': action, 'ttl_ms': ttl_ms})

    async def revoke(self, grant_id):
        return await self._bridge.request('grants.revoke', {'id': grant_id})

    async def list(self):
        return await self._bridge.request('grants.list', {})

    async def check(self, grant_id, scope, revision, policy, action):
        return await self._bridge.request('grants.check', {'id': grant_id, 'scope': scope, 'revision': revision, 'policy': policy, 'action': action})
