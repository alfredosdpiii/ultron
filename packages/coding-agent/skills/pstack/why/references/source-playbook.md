# Source playbooks

`why` searches source control plus every evidence category that has a configured MCP server. Each playbook below covers one category through the REPL. The named MCPs are examples: adapt the playbook to whichever server in that category is configured.

| Category | Playbook | Reached through |
|---|---|---|
| Source control history | [`code-archaeology.md`](./sources/code-archaeology.md) | `bash`: git, `gh` (always available) |
| Issue / ticket tracker | [`linear.md`](./sources/linear.md) | MCP: Linear, Jira, GitHub Issues (`gh issue` works without MCP), Shortcut |
| Long-form documents | [`notion.md`](./sources/notion.md) | MCP: Notion, Confluence, Google Docs, Coda |
| Real-time team chat | [`slack.md`](./sources/slack.md) | MCP: Slack, Discord, Teams, Mattermost |
| Infrastructure observability | [`datadog.md`](./sources/datadog.md) | MCP: Datadog, Grafana, Honeycomb, New Relic, Splunk |
| Error / exception tracking | [`sentry.md`](./sources/sentry.md) | MCP: Sentry, Rollbar, Bugsnag |
| Product analytics warehouse | no playbook | MCP: a SQL warehouse server, if configured |

For an analytics warehouse: probe the schema before trusting any table name (`SHOW TABLES`, `DESCRIBE`), use read-only queries, time-bound every query to a window around the change, and return compact numeric summaries (counts, percentiles, first and last seen), never raw rows. An event's existence shows someone cared to log it, not that the code exists because of it.

Cross-cutting:

- [`incident-postmortem.md`](./sources/incident-postmortem.md). Add its queries to every source when the target looks defensive (null checks, retries, timeouts, rate limits, feature flags, egress guards, OOM handlers).

## Reaching an MCP source

Tool names differ between servers and are often prefixed with the server name, so find the real ones before calling:

```python
server = "linear"  # from the coverage map
names = await mcp.tools(server)
print(names)
for match in await mcp.search("search issues by text", server=server, limit=5):
    print(match)
spec = await mcp.describe(names[0], server=server)
print(spec["parameters"])
```

Call with `r = await mcp.call(tool, {"query": "..."}, server=server)`; `r.json()` parses a JSON reply. An `McpError` for auth or connection is a gap: record it and move on, never guess what the source would have said.
