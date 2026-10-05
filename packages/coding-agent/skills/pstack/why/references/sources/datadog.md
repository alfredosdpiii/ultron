# Infrastructure observability (Datadog as the example)

Use only when an observability MCP server is configured (`await mcp.servers()`). Adapt for Grafana, Honeycomb, New Relic or Splunk.

## What this source contains

The runtime record: what actually happened in production, as opposed to what was planned.

- **Metrics.** A metric's existence is itself evidence that someone thought the number worth watching.
- **Monitors.** Conditions the team decided were worth waking someone for. A monitor on `rate_limit_hit > 10/min` is direct evidence the team worried about that threshold.
- **Dashboards.** What the team considers important for a subsystem.
- **Traces and spans.** For "why is this slow" and "why is there a timeout here".
- **Logs.** The error conditions that motivated defensive code.
- **Incidents.** Formal records with timelines and linked postmortems.

## Reaching it from the REPL

Find tools by kind: `await mcp.search("search monitors", server=server)`, `"dashboards"`, `"metric timeseries"`, `"search logs"`, `"incidents"`. Datadog's server names them along the lines of `search_datadog_monitors`, `search_datadog_dashboards`, `get_datadog_metric`, `search_datadog_logs`, `search_datadog_incidents`; confirm with `await mcp.tools(server)`.

Dashboards and monitors are independent searches; run them together:

```python
server = state["why"]["coverage"]["observability"]
query = "payment-service retry"
monitors, dashboards = await asyncio.gather(
    mcp.call("search_datadog_monitors", {"query": query}, server=server),  # real names from mcp.tools
    mcp.call("search_datadog_dashboards", {"query": query}, server=server),
    return_exceptions=True,
)
```

## How to search it

1. **Find the owning service** and its dependencies.
2. **Dashboards and monitors first**: they show what the team cares about. A monitor's threshold is often the answer to "why is this clamped at N?"
3. **Metrics around the target.** Correlate the trajectory with the change date: "the `payment_timeout` metric spiked on 2023-11-03; the retry logic merged on 2023-11-06."
4. **Logs: narrow, don't dump.** Always time-bound (about 30 days either side of the change); aggregate rather than pulling raw lines.
5. **Spans and traces** for timeouts, retries and cross-service behavior.
6. **Incidents** around the time defensive code was added.

Large replies go into `h = await rlm.load(text=str(r))` and are searched there, not printed.

## What good evidence looks like

- A monitor whose condition matches the constraint the code enforces
- A dashboard by the target's author with widgets for what the code guards
- A spike just before the merge and stable values after
- An incident record naming the target, its symbols or its error strings
- Logs showing the error pattern the code prevents, in the window before the change

## Common pitfalls

- **Correlation is not causation.** Check neighboring PRs in the same window.
- **Charts carry their author's framing.** A chart named "retry success rate" shows interest, not the reason for a line of code.
- **Vanished telemetry.** Renamed or expired metrics are a gap, not a null result.
- **Noise.** Narrow by service, tag and time.
- **Instrumented is not caused.** Cross-reference with commit dates.

## What to record

Per item: type (dashboard, monitor, metric, log pattern, trace, incident), name, ID or link, owner and dates, the condition, query or quote verbatim, and how strongly it connects to the target.
