"""Runtime hints (nano-rlm's rlm.hints): the host may end a cell's result with one line `[hint:<tag>] ...`
about how the runtime was used. A lane that has understood a hint mutes its tag so it is not repeated;
mutes are per lane and persist in the session.
"""


class Hints:
    def __init__(self, bridge):
        self._bridge = bridge

    async def mute(self, *tags):
        """Stop hints with these tags for this lane; returns the muted tags."""
        return (await self._bridge.request('hints.mute', {'tags': _tags(tags)}))['muted']

    async def unmute(self, *tags):
        """Re-enable hints with these tags; returns the tags still muted."""
        return (await self._bridge.request('hints.unmute', {'tags': _tags(tags)}))['muted']

    async def muted(self):
        """The tags currently muted for this lane."""
        return (await self._bridge.request('hints.muted', {}))['muted']

    def __repr__(self):
        return 'hints (await hints.mute(tag), hints.unmute(tag), hints.muted())'


def _tags(tags):
    if not tags or not all(isinstance(tag, str) and tag for tag in tags):
        raise TypeError('hint tags must be one or more non-empty strings')
    return list(tags)
