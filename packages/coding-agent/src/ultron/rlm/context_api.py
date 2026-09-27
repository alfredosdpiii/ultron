"""Model-owned context control (`ctx`): the model edits what it sees, never the durable transcript.

Every call acts on the calling lane's current conversation branch. Edits are Pi-style context edits:
they change only the model's view on this branch, and the transcript keeps every item.
"""


class Context:
    """`ctx` manages your own context; the durable transcript itself is never changed.

    - `await ctx.history(limit=20, kinds=None)` lists recent items with id, kind, bytes, preview and state.
    - `await ctx.get(id)` returns one item in full; `await ctx.forget(ids, reason)` removes items from what
      you see; `await ctx.summarize(ids, text)` replaces a span with your summary; `await ctx.pin(id)` keeps
      an item through edits and compaction; `await ctx.note(text)` records a note that survives compaction.
    - The current user message and pinned items cannot be forgotten.
    - Task results collapse on return: after you have seen a cell's output once, it shrinks to one line per
      finished task (definition, key, status, cost). `await agents.result("<task id>")` returns the full
      value again; `await ctx.get(id)` shows the original output.
    """

    def __init__(self, bridge):
        self._bridge = bridge

    async def history(self, limit=20, kinds=None):
        """Most recent items in the model's context (oldest first), each with id, kind, bytes, preview and state.

        kinds: optional list of "user", "assistant", "tool", "custom", "note", "summary", "compaction".
        """
        payload = {'limit': limit}
        if kinds is not None:
            payload['kinds'] = [kinds] if isinstance(kinds, str) else list(kinds)
        return await self._bridge.request('ctx.history', payload)

    async def get(self, id):
        """One item in full: the durable message and what the model currently sees of it."""
        return await self._bridge.request('ctx.get', {'id': id})

    async def forget(self, ids, reason):
        """Remove items from the model's context. The current user message and pinned items are refused."""
        return await self._bridge.request('ctx.forget', {'ids': [ids] if isinstance(ids, str) else list(ids), 'reason': reason})

    async def summarize(self, ids, text):
        """Replace a span of items with your own summary text (tool calls and their results move together)."""
        return await self._bridge.request('ctx.summarize', {'ids': [ids] if isinstance(ids, str) else list(ids), 'text': text})

    async def pin(self, id):
        """Protect an item from forget, summarize, collapse, and compaction."""
        return await self._bridge.request('ctx.pin', {'id': id})

    async def unpin(self, id):
        return await self._bridge.request('ctx.unpin', {'id': id})

    async def note(self, text):
        """Append a durable note to your context; it stays visible after compaction."""
        return await self._bridge.request('ctx.note', {'text': text})
