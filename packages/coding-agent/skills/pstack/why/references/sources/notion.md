# Long-form documents (Notion as the example)

Use only when a documents MCP server is configured (`await mcp.servers()`). Adapt for Confluence, Google Docs or Coda.

## What this source contains

- PRDs, technical specs, RFCs and ADRs
- Design review notes
- Postmortems and runbooks (often explain defensive code)
- Strategy documents that set priorities

The why is often written out here before it becomes code.

## Reaching it from the REPL

Find the search and fetch tools: `await mcp.search("search pages", server=server)`, `await mcp.search("fetch page content", server=server)`. Notion's server names them along the lines of `notion-search` and `notion-fetch`; confirm with `await mcp.tools(server)`.

Pages are long. Load each into a handle and search it before reading; give `rlm.map` frames only the sections a search line cannot settle:

```python
server = state["why"]["coverage"]["documents"]
hits = await mcp.call("notion-search", {"query": "retry backoff design"}, server=server)  # real name from mcp.tools
page = await mcp.call("notion-fetch", {"id": "<page id from hits>"}, server=server)
h = await rlm.load(text=str(page))
for m in h.search(r"(?i)(motivation|alternatives|decided|because)", limit=15):
    print(m["line"], m["text"])
```

## How to search it

1. **Keyword searches**: feature name, key symbols, author names, error strings, user-visible terms; time-bounded if the server supports it.
2. **Fetch candidate pages in full.** Rationale is often mid-document.
3. **Follow child pages and backlinks**: alternatives considered, appendices.
4. **Meeting-notes databases**, if the server exposes them.

## What good evidence looks like

- A "Problem statement" or "Motivation" section matching the target's purpose
- An "Alternatives considered" section
- A postmortem naming the target as the fix
- Meeting notes recording "we decided X because Y" in the PR's date range
- A filled-in ADR (status, context, decision, consequences)

## Common pitfalls

- **Outdated specs.** The doc describes the plan; the PR shows what shipped. Flag divergence.
- **Boilerplate templates.** Look for specificity.
- **Unlinked docs.** Broad keyword searches find them.
- **Multiple drafts.** Prefer the finalized or latest one; check dates.
- **No access** to a page is a gap.

## What to record

Per doc: title and URL, authors and last-updated date, the motivation quoted verbatim with its section, linked pages, finalized or draft.
