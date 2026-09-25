# Performance: Ultron overhead vs stock Pi

The quality comparison (`scripts/eval-quality.mjs`, 2026-09-25, `cliproxyapi/gpt-6-sol`) found Ultron's median
wall time per task at 1.75x Pi's (92 s vs 53 s) at equal total cost. This page records where Ultron's own
host work goes, what was changed, and what the measurements say about the rest of the gap.

## Method

`scripts/profile-overhead.mjs` runs both agents in RPC mode with `--no-session` (like the quality eval)
against a local fake OpenAI-compatible provider, each run in a fresh isolated agent dir that holds only a
`models.json`. The fake model has the same compat flags and shape as the eval's `gpt-6-sol` entry. Model
latency is zero unless a scenario adds it, so everything measured is host work.

```
node scripts/profile-overhead.mjs --runs 7 --variants pi,bundle:/path/to/base-checkout,bundle \
  --scenario text,bash,rlm,edit [--dump-system DIR]
```

Variants: `pi` (installed stock Pi 0.84.4), `ultron` (the linked build), `source` (this checkout's
source CLI), `bundle` (this checkout's `dist/bundle` after `npm run build`), and `bundle:<checkout>` (another
checkout's bundle, e.g. a `git archive` of the base commit). Scenarios: `text` (two text-only prompts),
`bash` (one bash call, then an answer), `rlm` (two prompts that each run Python: a cold kernel, then a warm one),
`edit` (read, edit, run, answer with 1 s model latency per request), `stream`/`paced` (4000 streamed deltas,
unpaced or 2 ms apart), and `retry` (the first request fails with the proxy's `500 empty_stream`).
`ULTRON_STARTUP_TRACE=<file>` makes every Ultron process append startup milestones; the script reports them
relative to spawn. `PROFILE_SESSION_HISTORY=50x4` seeds 50 earlier 4 MiB sessions into the profile.

"Before" is the base commit `dcff3e24a` built the same way. All numbers are medians of 7 runs on the same machine,
in milliseconds.

## Findings

### Requests are the same size

| First request | Pi 0.84.4 | Ultron |
| --- | --- | --- |
| Body bytes | 8965 | 8655 |
| System prompt chars | 5734 | 5048 |
| Tools | 4 (2901 bytes) | 5 incl. `rlm` (3274 bytes) |
| Parameters | `stream`, `stream_options`, `store:false`, `max_completion_tokens`, `reasoning_effort` | identical |

Later requests in a tool loop carry identical messages, the system prompt and tool list stay byte-stable
across turns (so prefix caching is unaffected), and Ultron makes no extra model calls: a one-tool task makes
2 requests in both. Prompt size is not a source of latency, and there is no duplicated prompt content to trim.
Ultron's prompt is shorter because its worker passes fixed one-line tool snippets and no tool guidelines, so it
drops Pi's edit and read guidance. That affects behavior, not host time.

### Startup: 150 to 170 ms over Pi, now about 100 ms

Where a fresh Ultron RPC session spends its startup (bundle, after the changes):

| Milestone (ms after spawn) | |
| --- | --- |
| CLI modules loaded | 184 |
| Coordinator process spawned and listening | +55 |
| Server backend, RPC client, session created | +35 |
| Session worker process boot to entry (module load of the 3.4 MiB worker chunk) | +200 |
| Worker: model registry, resources, harness, RLM host | +45 |
| Attached, ready for the first prompt | +10 |

Stock Pi is ready in about 300 ms. It loads one process; Ultron starts three (CLI with in-process server,
coordinator, session worker), and the worker's module load is on the critical path.

### Per turn: under 10 ms

| | Pi | Ultron |
| --- | --- | --- |
| Prompt to first provider request, first prompt | 19 | 24 |
| Prompt to first provider request, second prompt | 1 | 4 |
| Text-only turn, end to end | 2 to 3 | 8 to 10 |
| One bash call turn, end to end | 34 | 54 |
| Tool end to follow-up request | 2 | 2 to 3 |
| Edit task, 4 requests at 1 s model latency | 4094 | 4107 |

The remaining per-turn cost is the durable task journal: a one-tool task commits about 20 journal batches
(assistant frames, operation state, tool arguments and outputs, usage). These are the commit-before-publish
guarantees and stay as they are. With Hindsight off (as in the eval) no Jev or memory call is made
automatically. Jev runs only when agent code calls `jev.*` or memory, so there is no per-turn Jev or memory-gating
latency.

### RLM kernel: 65 ms on the first call

The first `rlm` call in a session spawns the Python kernel: 66 ms of tool time, against 1 to 2 ms for a warm
call. It is paid once per session, only when `rlm` is used, so prewarming was not worth a Python process for every
session.

### Streaming relay: CPU-bound only when unpaced

Relaying 4000 unpaced deltas takes 1.0 s in Ultron against 0.2 s in Pi. The CPU goes to the agent harness event
bus, which `structuredClone`s the whole partial message once per emit and again per listener
(`packages/agent/src/harness/events.ts`), and to chord state diffs and schema checks in the client. At a realistic
pace (one delta per 2 ms) the difference is gone: 8376 ms against 8336 ms. This is quadratic in message length,
but it is not a wall-time cost at model streaming rates.

### Provider retries

A transient `500 empty_stream` costs both agents the same one-second backoff before the retry succeeds.

## Changes

1. **Compile cache for internal processes.** Only `dist/bundle/cli.js` enabled Node's compile cache.
   The coordinator, server and session-worker entries loaded about 4 MiB of bundled code without it, on every new
   session. The bundle now emits `*-entry.js` launchers that enable the cache and import `*-entry-runtime.js`
   (`scripts/build-coding-agent-bundle.mjs`). Worker module load drops from about 195 ms to 120 ms when run on its own.
2. **No session scan on a fresh start, and header-only reads.** `ultron` read every earlier session file in full
   just to parse its first line, on every start, including `--no-session`. It now scans only when a selector
   (`--continue`, `--resume`, `--session`, `--fork`) needs the list, and reads only up to the first newline
   (`src/native-command.ts`).
3. **Pi's HTTP stack in the session worker.** Model requests leave from the session worker. That process never
   called `configureHttpDispatcher`, so it used Node's bundled fetch without Pi's idle timeout setting or
   `HTTP(S)_PROXY` support. It now installs the same dispatcher as the Pi CLI (`src/experimental/session-worker.ts`).
4. **Startup tracing.** Adds `ULTRON_STARTUP_TRACE` (`src/experimental/startup-trace.ts`), which is off unless set.

## Before and after

| Scenario (median ms) | Pi 0.84.4 | Ultron before | Ultron after |
| --- | --- | --- | --- |
| Ready (text) | 301 | 467 | 402 |
| Whole run, two text prompts | 336 | 551 | 478 |
| Ready (bash) | 303 | 450 | 422 |
| Whole run, one bash call | 345 | 537 | 503 |
| Whole run, two rlm prompts | n/a | 615 | 563 |
| Whole run, edit task (4 s of model time) | 4490 | 4706 | 4552 |
| Ready with 50 x 4 MiB earlier sessions | n/a | 673 | 469 |

Startup overhead over Pi fell from about 165 ms to about 100 ms. In a long-used profile it fell by 200 ms or more,
and that saving grows with session history.

## What this means for the 1.75x

Ultron's host overhead is about 0.1 to 0.25 s per task. The eval's gap is tens of seconds per task at the same
tool calls, tokens and cost. For example, `edit-average` took 43 to 49 s in Ultron against 16 to 20 s in Pi,
with 2 to 3 tool calls, about 6.8k tokens and about $0.007 each. Host work does not explain that gap. The time is
spent between Ultron's request and the proxy's response, not in Ultron.

The same run also shows five terminal `500 empty_stream: upstream stream closed before first payload` errors for
Ultron and none for Pi. The proxy (CLIProxyAPI with `request-retry: 3`) retries empty upstream streams internally
before it gives up. Those hidden retries cost wall time but no tokens, which matches the pattern: same cost, longer
wall time, and occasional surfaced failures.

Leads to check with metered runs (not done here):

- Per-request time to first byte and total stream time for both agents, taken from proxy logs, to confirm that
  the gap is proxy-side retries.
- Request differences that could make upstream empty streams more likely: the `rlm` tool definition, the
  prompt that lacks Pi's tool guidelines, and the runtime (Pi 0.84.4 runs its own Node 24; Ultron runs the
  system Node 26).
