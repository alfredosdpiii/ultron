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

# Cost pass (2026-09-27): where the research task's 8M tokens went

On `cliproxyapi/gpt-6-sol` the research task `incident-root-causes-full` (400 reports, 2.1 MB; find the
expired-certificate root causes) was exact for both agents, but Ultron took 369 s and 8.0M tokens ($4.48) against
Pi's 71 s and 289k tokens ($0.17). On the 15-task hard set Ultron used 1.55x Pi's tokens at 1.46x its median time.
This section breaks the research run down, records what changed, and measures the result live.

## Evidence

- Recorded run A (`acceptance/quality/2026-09-27-research-cliproxyapi_gpt-6-sol.json`, main checkout): the root's
  RPC events, plus `inspect agents.status` and `get_session_stats` at the end.
- Diagnostic run B (`acceptance/quality/2026-09-27-research-cliproxyapi_gpt-6-sol-ultron-diagnostic-before.json`):
  the same code, Ultron only, with the new `--keep-all` eval option. It showed the same pattern. With
  `--no-session` the child lanes' transcripts were not written to disk, and the usage ledger records child model
  calls without tokens (`unknownCalls`). So per-child usage is reconstructed: session totals minus the root's
  per-message usage. `--keep-all` now also drops `--no-session`, so later runs keep every lane's transcript and the
  frame traces.

## Breakdown

| | Pi | Ultron run A | Ultron run B |
| --- | --- | --- | --- |
| Wall time | 71 s | 369 s | 196 s |
| Total tokens (cost) | 288,678 ($0.17) | 7,995,811 ($4.48) | 4,359,699 ($2.49) |
| Model requests | 12 | 339 | 242 |
| Root requests / tokens | 12 / 289k | 13 / 164k (2.1%) | 13 / 231k (5.3%) |
| Subagents (`rlm.spawn`) | none | 24 `rlm-child`, 3 levels deep (4, then 16, then 4) | 25 `rlm-child`, 4 levels deep (4, 9, 11, 1) |
| Their requests / tokens | | 326 / 7.83M (97.9%) incl. 9 frames | 229 / 4.13M (94.7%) |
| Inference frames | | 9 `rlm-frame` (3 answered, 6 refused: "the budget cannot cover the first request") | 0 |
| Root time blocked in `rlm.collect` | | 284 s (77%) | 116 s (59%) |
| Cache reads | 0.20M | 6.71M (84% of tokens) | 3.67M (84%) |

So the cost came from subagents, not from frames or the root. Frames were a rounding error: the 9 in run A were
started by grandchildren, and six of them spent nothing. What happened in both runs:

1. The root spawned subagents at its third or fourth cell, before it had narrowed anything. In run A that was
   4 children of 100 reports each. In run B it filtered to 146 keyword candidates, split them over 3 children,
   and sent the other 254 reports to a fourth.
2. Every child is a full agent. It has the same system prompt, runtime guide and tool schema, about 5.8k tokens
   per request, and re-sends its growing transcript on every turn. Each child split its slice again into
   grandchildren, and some grandchildren split again. The guide said nothing to stop this, and nothing in the
   host limited it.
3. Meanwhile the root did Pi's winning approach itself. It grepped, printed one line per candidate and read the
   root-cause lines of about 30 candidates. Then it waited for the children and wrote their answer, so the tree's
   work was duplicated.

The fixed per-request prompt alone accounts for 326 x 5.8k = 1.9M tokens in run A (24%) and 1.33M in run B
(31%). The rest is the children's transcripts: each read 25 to 100 reports (about 1.4k tokens each) and re-sent
them on every later turn, about 24k tokens per request on average in run A.

Pi's run had 12 requests. It counted regex hits for `expir`, `cert`, `TLS`, `SSL` and `notAfter`, and looked at
the heads of 5 reports. It printed the certificate-related lines of every matching report, one compact line per
file (44 KB), and checked expiry mentions without certificate words. It printed the root-cause and expiry lines of
35 candidates (36 KB), then wrote the 25 ids. Its context peaked at about 45k tokens.

Other findings:

- Both agents' first cell read `~/.agents/skills/unslop/SKILL.md` ("Must always apply"). The eval's isolated
  profile does not isolate `~/.agents/skills`, so both prompts list the user's skills (about 0.9k tokens).
- The root's rlm output adds no framing: a cell's result is its printed text, plus at most one hint line.
- The system prompt and tools are byte-identical across turns (checked on every request of the `rlm` profile
  scenario, before and after), so prompt caching works. The cost is volume, not cache misses.

## Fixed cost per request

Measured with `scripts/profile-overhead.mjs --scenario rlm,text --dump-system` (local fake provider, same machine
profile, so both prompts include the same user skills list):

| First request | Pi 0.84.4 | Ultron before | Ultron after |
| --- | --- | --- | --- |
| Request body | 8,965 B | 22,310 B | 16,681 B |
| System prompt | 5,734 chars | 19,393 chars | 14,879 chars |
| of which the runtime guide | | 13,976 chars | 9,462 chars |
| Tool schema | 2,901 B (4 tools) | 2,447 B (`rlm`) | 1,372 B (`rlm`) |
| Live first-request input tokens (gpt-6-sol) | 2,349 | 5,826 | 4,535 |

## Changes

1. **Search before delegating** (`src/ultron/rlm/prompt.ts`). A new guide section: for a corpus, search for the
   concept and its synonyms in Python, print one line per candidate, read the deciding passages of the unclear
   ones, and use `rlm.map` only for what a line cannot settle or when the narrowed text is still over about 100 KB.
   Never spawn subagents to read or classify documents. It includes a worked example shaped like Pi's approach.
   The Delegation section says the same, and tells a subagent to do its brief itself.
2. **Subagent nesting limit** (`native-host.ts`). `rlm.spawn` refuses beyond `ULTRON_SPAWN_DEPTH` levels (default
   3 since 0.87.14, a ceiling; below it a subagent nests only when its parent passed `depth=N`, default 0; it was a
   flat 2 until 0.87.10 and 1 in 0.87.11 to 0.87.13; 0 means no limit). The error tells the
   model to do the part itself. On the deep delegation task, children that split their service across grandchildren
   raised the cost without finishing sooner.
   Both runs above went 3 and 4 levels deep.
3. **Cheaper, bounded frames** (`inference.ts`, `infer_api.py`):
   - A top-level `rlm.map` without a token limit gets a default budget of 500,000 tokens (`ULTRON_RLM_MAP_TOKENS`).
     Frames beyond it come back `Incomplete`, with a detail that says how to raise the limit.
   - `rlm.map` returns a `MapResults` list with `.spent`, `.budget` and `.remaining`, and prints one line such as
     `[rlm.map] 40 frames: 38 complete, 2 incomplete, 0 failed; spent 40 calls, 81,200 tokens of 500,000`, so
     the model sees what the map cost. It snapshots as a plain list.
   - Without `max_repairs`, a scalar contract (int, float, bool, str, null) is re-asked once instead of twice.
   - The per-frame `Context: N view(s), X characters` line before the views is gone. A map's frames now share the
     task and the shared `context=` views as a byte-identical, cacheable prefix.
   - The frame system prompt was already about 500 characters with no tools, so it is unchanged.
4. **Shorter guide and tool description.** Runtime, skills, bounded inference and delegation are compressed to one
   line per API. The details of `ctx`, code skills, agents as classes, typed agents, workflows, background jobs,
   shell jobs and `rlm` moved into their docstrings, which the guide points to with `help(obj)`. The guide went from
   13,976 to 9,462 characters, including the new 1.4k search section. The `rlm` tool description went from 2,185 to
   1,117. Every root and child request is about 1.3k tokens lighter.
5. **`--keep-all`** in `scripts/eval-quality.mjs` keeps passing runs' evidence and the session.

A47 to A50 behaviour is unchanged: contracts, repair, `Incomplete`, shared budgets, tranches and cancellation. An
explicit `max_repairs` or token budget always wins. The A47/A48 evidence tests gained assertions for the reported
spend and were relocked.

## Live results (gpt-6-sol, 1 trial, Ultron only; Pi's recorded runs are the baseline)

Research, `incident-root-causes-full`
(`acceptance/quality/2026-09-27-research-cliproxyapi_gpt-6-sol-ultron-after-cost-pass.json`):

| | Pi | Ultron before (A / B) | Ultron after |
| --- | --- | --- | --- |
| Pass (precision, recall) | 1.0, 1.0 | 1.0, 1.0 / 1.0, 1.0 | 1.0, 1.0 |
| Wall time | 71 s | 369 s / 196 s | 73 s |
| Tokens | 288,678 | 7,995,811 / 4,359,699 | 286,685 |
| Cost | $0.167 | $4.48 / $2.49 | $0.143 |
| Requests, subagents, frames | 12, 0, 0 | 339, 24, 9 / 242, 25, 0 | 12, 0, 0 |

After the change, Ultron searched and narrowed in 11 cells, much as Pi did. It printed compact candidate lines and
read about 25 candidates' root-cause passages, then wrote the answer. It spawned nothing and used no frames.

Hard set, 5 representative tasks
(`acceptance/quality/2026-09-27-hard5-cliproxyapi_gpt-6-sol-ultron-after-cost-pass.json`; Pi and "before" are from
`2026-09-27-hard-cliproxyapi_gpt-6-sol.json`):

| Task | Pi tokens / time | Ultron before | Ultron after |
| --- | --- | --- | --- |
| bugs-scheduler | 87,735 / 117.7 s | 233,426 / 108.7 s | 51,744 / 70.0 s |
| refactor-intervals-fast | 31,084 / 71.4 s | 114,205 / 183.8 s | 70,339 / 145.7 s |
| data-sessions | 31,421 / 32.8 s | 66,540 / 55.8 s | 34,682 / 34.5 s |
| logs-bruteforce | 23,466 / 28.5 s | 32,039 / 31.3 s | 34,927 / 39.1 s |
| huge-catalog-diff | 17,382 / 27.9 s | 31,822 / 35.6 s | 24,915 / 35.5 s |
| Total tokens (cost) | 191,088 ($0.235) | 478,032 ($0.481) | 216,607 ($0.285) |
| Median time | 32.8 s | 55.8 s | 39.1 s |
| Passed | 5/5 | 5/5 | 5/5 |

On these five tasks, Ultron's tokens fell by 55%, from 2.50x Pi's to 1.13x. Its median time went from 1.70x to
1.19x Pi's. These are single trials, so the per-task time differences are within run-to-run noise. The token
totals are the more reliable signal.

## Left (addressed below)

- Child model calls were recorded in the usage ledger without tokens (`unknownCalls`), so `ULTRON_MAX_TOTAL_TOKENS`
  and the cost cap could not govern an `rlm.spawn` subtree. Only admission and wall time bounded it.
- The eval's isolated profile still listed `~/.agents/skills`, for Pi and Ultron alike.
- Ultron's first request was still about 2x Pi's (4.5k against 2.3k tokens).

# Limits and prompt pass (2026-09-27)

## Tree limits

Every assistant response on every lane (the root's own, `rlm.spawn` children at any depth, frames, typed agents,
background jobs) is now charged, tokens and cost, to the root that admitted its lane. The turn limit, the token
limit (`ULTRON_MAX_TOTAL_TOKENS`, `rootBudget`) and the cost cap (`ULTRON_MAX_COST_USD`) all read that tally, and
`before_request` refuses the next model request on any lane of a spent tree, the root's own included. A task
stopped this way fails with the limit message; `agents.status` shows the tree's `spend`. Before, a runaway child
under a $0.02 cap made 1,850 requests (the cap only saw settled calls); now it stops at the request that crosses
the cap. Each response counts once: frames and children also settle a model call in the ledger, but a root's spend
is its per-response tally, and a frame's `Budget` is a nested cap inside it. `test/ultron-tree-budget.test.ts`
runs the real CLI against a scripted provider for the token, cost and turn limits and the frame accounting.

## Eval isolation

`scripts/eval-quality.mjs` and `scripts/profile-overhead.mjs` now give each agent an empty `HOME` inside the run's
work dir. Both Pi and Ultron read `~/.agents/skills` from the home directory, so the agent dir alone did not
isolate them. The result JSON records `isolation`; `scripts/eval-isolation.test.mjs` checks both agents with a
canary skill. The user's skills list was about 3.2 KB of every request for both agents.

## First request

`node scripts/profile-overhead.mjs --runs 1 --variants pi,source --scenario text`:

| First request | Pi 0.84.4 | Ultron before | Ultron after |
| --- | --- | --- | --- |
| Body, user home (skills listed) | 8,965 B | 16,642 B | 9,844 B |
| Body, isolated home | 5,755 B | 13,408 B | 6,629 B (1.15x Pi) |
| System prompt, isolated | 2,567 chars | 11,666 chars | 5,633 chars |
| of which the runtime guide | | 9,462 chars | 5,084 chars |
| of which Pi's docs pointers and custom-tools note | 1,262 chars | 1,329 chars | omitted |
| Tool schema | 2,901 B (4 tools) | 1,372 B | 689 B |

Guide sections, before and after (characters): Runtime 1,974 to 1,261 (with async events), Skills 2,097 to 1,038,
Search before delegating 1,393 to 606, Bounded inference 1,723 to 786, Delegation 1,336 to 752, Other APIs 934 to
636. How:

1. Pi's docs pointers (paths to Pi's README, docs and examples) and the "other custom tools" note are left out in
   the REPL-only default (`includeHarnessDocs: false` in `buildSystemPrompt`); native mode keeps them.
2. Each section keeps its rules in one line per API: search before delegating, never spawn subagents to read
   documents, turn endings, output limits, project code through `bash` with the project's interpreter, `edit`
   semantics, no polling. The worked search example moved to `help(rlm)`, the `rlm.map` example and handle and
   contract detail to `help(rlm.load)`/`help(rlm.infer)`/`help(rlm.map)`, and the spawn and collect detail to
   `help(rlm.spawn)`/`help(rlm.collect)`.
3. The `rlm` tool description only names the pre-imported APIs; the tool line and REPL rule in Pi's lists are one
   short line each. The skills are described once, in the guide.

The prompt stays byte-stable across turns (checked on the second request).

## Live checks (glm-5.3-flash, thinking max, 1 trial, Ultron only, isolated home)

| Task | Earlier glm runs (Ultron) | After |
| --- | --- | --- |
| research `incident-root-causes` (157 reports) | pass, 353,509 tokens, 273 s, 12 cells, 99 frames (2026-09-26) | pass (P 1.0, R 1.0), 244,540 tokens, 208 s, 14 cells, 0 frames |
| hard `bugs-scheduler` | pass, 273,984 / 310,373 tokens, 517 / 361 s (2026-09-26) | pass, 214,535 tokens, 630 s, 10 cells |
| hard `data-sessions` | pass, 26,358 / 26,141 tokens, 69 / 52 s | pass, 18,384 tokens, 90 s, 3 cells |
| hard `logs-bruteforce` | pass, 44,661 / 53,329 tokens, 83 / 89 s | pass, 51,521 tokens, 129 s, 8 cells |

All four passed, with 529k tokens in all against 721k for the earlier runs (the research run plus the means of
the hard pairs). The longer wall times come from the model: most of `bugs-scheduler`'s 630 s is
one 455 s response with thinking at max, while the three hard runs and the research run shared the proxy.
In the research run the model searched before reading, as the guide asks: it globbed the corpus, filtered with
two regexes (certificate terms and expiry terms), printed compact per-candidate lines, read the deciding passages
of the unclear ones, and wrote the answer, with no subagents and no frames. Results are in
`acceptance/quality/2026-09-27-research-cliproxyapi_glm-5.3-flash-thinking-max-compact-prompt.json` and
`acceptance/quality/2026-09-27-hard-cliproxyapi_glm-5.3-flash-thinking-max-compact-prompt.json`.
