# Error tracking (Sentry as the example)

Use only when an error-tracking MCP server is configured (`await mcp.servers()`). Adapt for Rollbar or Bugsnag.

## Reaching it from the REPL

Find the tools for organizations and projects, issue search, issue events, a single resource, and releases: `await mcp.search("search issues", server=server)`, `await mcp.search("releases", server=server)`. Sentry's server names them along the lines of `find_organizations`, `find_projects`, `search_issues`, `search_issue_events`, `get_issue_tag_values`, `get_sentry_resource`, `find_releases`; confirm with `await mcp.tools(server)`.

```python
server = state["why"]["coverage"]["error tracking"]
queries = ["TimeoutError in retryWithBackoff", "unhandled exceptions in uploadFile"]
issues = await asyncio.gather(*(mcp.call("search_issues", {"naturalLanguageQuery": q}, server=server) for q in queries),
                              return_exceptions=True)  # tool and argument names from mcp.describe
```

## How to search it

1. **Orient**: organization and project slugs.
2. **Search issues** with exception class names the target handles, the target's function or class, error strings it checks for, its file path.
3. **Narrow by time and release.** For each candidate: first seen, last seen, affected releases, frequency trajectory. Does the end line up with the target's ship date?
4. **Pull a full event.** Does the stack trace pass through the target? Do tags and breadcrumbs match the condition it defends against?
5. **Releases near the target's merge date.** Cross-reference with the PR.
6. **AI root-cause summaries** (such as Seer) are hypotheses, not evidence. Events and timestamps are the evidence.

## What to record

Per issue: ID and title, project, first and last seen, event count (and sampling if known), affected releases, a verbatim stack-trace excerpt showing relevance, the correlation with the ship date, the link, any author comments.
