# Error tracking (Sentry as the example)

Use only when an error-tracking MCP server is configured (`await mcp.servers()`). Adapt for Rollbar or Bugsnag.

## What this source contains

The archive of things that went wrong. For defensive, corrective or error-handling code it often holds the direct motivation: the exceptions, stack traces and frequencies that made someone add a check, catch, retry or fallback.

- Issues: grouped errors with counts, first / last seen, affected releases, comments
- Events: individual occurrences with stack traces, tags, breadcrumbs
- Releases: which version shipped with which issues

Its main value is temporal correlation: "the issue started 2024-01-02, peaked at 500 events a day, and stopped after the release that shipped the check."

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

## What good evidence looks like

- An issue first seen shortly before the target's PR and last seen shortly after
- Stack traces landing on the target function
- An issue comment from the PR author describing the fix
- The PR or commit citing the issue URL or ID
- A high-volume issue that stops with the release containing the target

## Common pitfalls

- **Grouping drift.** Renames can regroup the same error under a new issue. If an issue ends abruptly, look for a new one right after.
- **Release correlation is noisy.** A release holds many commits; cross-reference the exact commit.
- **Silent fixes.** The error may stop because upstream changed.
- **Resolved is not fixed.** It is a human marker.
- **Sampling.** Low counts may mean heavy sampling. Note it.

## What to record

Per issue: ID and title, project, first and last seen, event count (and sampling if known), affected releases, a verbatim stack-trace excerpt showing relevance, the correlation with the ship date, the link, any author comments.
