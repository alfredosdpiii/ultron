# Team chat (Slack as the example)

Use only when a chat MCP server is configured (`await mcp.servers()`). Adapt for Discord, Teams or Mattermost.

## What this source contains

- Real-time discussion of problems and decisions
- Incident channels where fire-drill decisions were made
- Design threads where tradeoffs were debated
- Answers from senior engineers that never reached a doc

Often where smaller decisions were actually made, and the most ephemeral source: retention, archived channels and unsearchable DMs limit it.

## Reaching it from the REPL

Find the message-search and thread tools: `await mcp.search("search messages", server=server)`, `await mcp.search("thread replies", server=server)`; confirm names with `await mcp.tools(server)` and arguments with `await mcp.describe(tool, server=server)`. If the server reports auth required, stop and record the gap.

Run the independent searches together:

```python
server = state["why"]["coverage"]["team chat"]
search = "search_messages"  # the real name from mcp.tools(server)
queries = ["retryWithBackoff", "/pull/4821", "from:@author retry"]  # symbols, PR URL, author around the merge date
hits = await asyncio.gather(*(mcp.call(search, {"query": q}, server=server) for q in queries), return_exceptions=True)
for q, r in zip(queries, hits):
    print(q, "->", (str(r)[:300] if not isinstance(r, Exception) else f"error: {r}"))
```

## How to search it

1. **Author-bounded**: the PR author's messages around the merge date. Narrow and often decisive.
2. **Keywords**: feature name and key symbols, including casual phrasings.
3. **PR URL**: PRs are linked when reviewed or discussed (`/pull/<number>`).
4. **Error strings** the code handles: incident threads surface.
5. **Channel-scoped**: engineering, project, incident and owning-team channels.
6. **Fetch the whole thread** for every relevant hit. The decision lives in the replies.

## What good evidence looks like

- A thread debating tradeoffs ("I was going to use A but B is better because ...")
- An incident message describing the bug the code prevents
- A reviewer's question with an authoritative answer from the author or lead
- A product or customer-facing message explaining a customer ask

## Common pitfalls

- **Retention cliffs.** Note the date before which nothing is found.
- **DMs** are not searchable. A known limitation.
- **Jokes are not decisions.** Look for considered discussion.
- **Single messages out of context** mislead. Always read the thread.
- **Auth failures**: report the gap; never invent findings.

## What to record

Per thread: channel, permalink, participants, date range, the key quotes verbatim with attribution, and what discussion or incident it belonged to.
