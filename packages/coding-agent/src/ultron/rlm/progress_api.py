"""Evidence-based progress receipts and reassessment. The host derives the reporter from the lane."""


def _payload(**fields):
    return {key: value for key, value in fields.items() if value is not None}


class Progress:
    def __init__(self, bridge):
        self._bridge = bridge

    async def report(self, summary, evidence, *, task_id=None, metrics=None):
        return await self._bridge.request('progress.report', _payload(task_id=task_id, summary=summary, evidence=evidence, metrics=metrics))

    async def assess(self, task_id):
        return await self._bridge.request('progress.assess', {'task_id': task_id})

    async def reassess(self, task_id, *, extend_budget=None, claim=None, verifier=None, verifier_input=None):
        return await self._bridge.request('progress.reassess', _payload(task_id=task_id, extend_budget=extend_budget, claim=claim, verifier=verifier, verifier_input=verifier_input))

    async def history(self, task_id):
        return await self._bridge.request('progress.history', {'task_id': task_id})
