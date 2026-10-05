# Issue tracker (Linear as the example)

Use only when an issue-tracker MCP server is configured (`await mcp.servers()`). For GitHub Issues, `gh issue view <n> --json title,body,comments,labels` and `gh search issues '<query>' --repo <owner/repo>` through `bash` work without MCP. Adapt for Jira or Shortcut.

## What this source contains

- Issues describing features and bugs, and their motivation
- Comments with clarifications, scope changes and "why we're doing this"
- Parent / sub-issue trees (initiative to ticket) and project docs (PRDs, specs)
- Labels (`compliance`, `customer-request`, `perf`) that signal the kind of motivation
- Linked PRs

This is where the product and business forcing function usually lives: "customer X asked", "this is for the Q3 compliance work".

## Reaching it from the REPL

Find the server's tools for fetching one issue, text search, and projects: `await mcp.search("get issue", server=server)`, `await mcp.search("list issues text query", server=server)`. Linear's server names them along the lines of `get_issue`, `list_issues`, `get_project`; confirm with `await mcp.tools(server)` and check arguments with `await mcp.describe(tool, server=server)`.

```python
server = state["why"]["coverage"]["issue tracker"]
ids = ["ENG-1234"]  # ticket IDs from commit messages and PR bodies
get_issue = "get_issue"  # the real name from mcp.tools(server)
tickets = await asyncio.gather(*(mcp.call(get_issue, {"id": i}, server=server) for i in ids), return_exceptions=True)
```

## How to search it

1. **Linked tickets first.** IDs from the seed commits and PRs. Read the full issue including comments.
2. **Keyword search** for the feature name, key symbols and business terms, in several phrasings.
3. **Walk the tree.** Sub-issues are tactical; the parent often carries the why.
4. **Project docs.** Specs and rationale attached to the project.
5. **Labels and milestones.** They hint at the kind of motivation and the deadline behind it.

## What good evidence looks like

- A description stating the business problem ("Customer Acme needs X for their SOC2 audit")
- A comment recording a decision and its reason
- A parent issue named like an initiative ("Reduce Payment Failures")
- An attached PRD or spec
- Labels like `incident-followup`, `compliance`, `perf-regression`

## Common pitfalls

- **Scope drift.** Tickets get closed and reopened with a different scope. Read the history.
- **Boilerplate "Why" sections** ("improve user experience") are not an answer.
- **Stale tickets** describe a plan that changed. Compare dates with the ship date.
- **Duplicate chains.** Follow them back to the canonical ticket.
- **No access** to an issue is a gap, not a guess.

## What to record

Per ticket: ID and title, the motivation quoted verbatim from description or comments, labels, parent, project, author, created and closed dates, link.
