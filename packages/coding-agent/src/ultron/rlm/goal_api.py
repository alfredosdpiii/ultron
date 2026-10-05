"""The session goal (`/goal`): a person sets an objective and a background job works on it in its own REPL until
it is done. Only the person changes it; the job (or the root) tests it with the host's check and ends it, naming
the revision it worked on."""


class Goal:
    def __init__(self, bridge):
        self._bridge = bridge

    async def get(self):
        """The goal (objective, revision, status, rounds, check command, last check run), or None."""
        return await self._bridge.request('goal.get', {})

    async def check(self):
        """Run the goal's check command on the host: {"passed", "exit_code", "timed_out", "output", "paused"}.
        Ten failing checks in a row with the same result pause the goal ("paused": True)."""
        return await self._bridge.request('goal.check', {})

    async def complete(self, revision, summary, evidence):
        """Declare the goal done. evidence: non-empty list of strings (commands with their outcome, files with
        lines). With a check command the host runs it now: the goal completes only if it exits 0, else the
        result says why and the goal stays active. Returns {"complete": bool, "status": ..., "check": ...}."""
        return await self._bridge.request('goal.complete', {
            'revision': revision, 'summary': summary, 'evidence': list(evidence),
        })

    async def blocked(self, revision, reason):
        """Declare that something outside you blocks the goal (with a check command: after 3 checks)."""
        return await self._bridge.request('goal.blocked', {'revision': revision, 'reason': reason})

    def __repr__(self):
        return 'goal (await goal.get(), goal.check(), goal.complete(revision, summary, evidence), goal.blocked(revision, reason))'
