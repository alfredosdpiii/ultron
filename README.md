# Ultron

**A terminal coding agent that programs its way through your work instead of picking tools from a menu.**

Most coding agents give the model a list of tools (read a file, run a command, edit, search) and let it call
them one at a time, pasting every result into its context. Ultron gives the model one thing: a persistent Python
REPL. Shell commands, edits, file reads, your MCP servers, sub-agents and sub-model calls are all Python functions
inside it. The model writes a cell that loops, filters, fans out and keeps the results in variables, and only what
it prints comes back into its context.

It is a fork of [Pi](https://github.com/badlogic/pi-mono) and keeps Pi's interface, so the TUI, `-p` print mode,
`--mode json`, `--mode rpc`, providers, extensions, skills and prompt templates all work the same.

```bash
npm install -g --ignore-scripts https://github.com/alfredosdpiii/ultron/releases/latest/download/ultron.tgz
```

## What makes Ultron different

### 1. The REPL is the only tool

The model's tool list is exactly one entry, `rlm`. Everything else is a pre-imported async function:

```python
out = await bash('''pytest -q tests/test_parser.py''')          # shell, output as a Python string
await edit("src/parser.py", old_str=OLD, new_str=NEW)           # exact edit that fails loudly if stale
runs = await asyncio.gather(*(mcp.call("exa-agent_exa_agent_create_run", query=q)   # your MCP servers
                              for q in questions))
```

With separate tools available, models take the familiar route (a shell call, a file write) and never use the REPL.
Measured on our hard task set, REPL use went from 0 of 16 runs to 29 of 30 once the REPL was the only door. This is
the design of Prime Intellect's RLM harness, including its choice to expose MCP tools as Python skills.

### 2. Big inputs never enter the model's context

```python
h = await rlm.load("logs/app.log")                    # a handle: size, digest, never the text itself
hits = h.search(r"ERROR .*timeout", limit=50)
causes = await rlm.map("Root cause in 10 words.",      # one bounded sub-model call per slice
                       [h.lines(m["line"] - 20, m["line"] + 5) for m in hits],
                       contract=str, budget=Budget(calls=60))
```

`rlm.infer` and `rlm.map` run private sub-model frames that see only the slices you pass them, return values
validated against a JSON-schema contract (malformed answers are re-asked), and draw on one shared budget of calls,
tokens and depth. Running out returns an `Incomplete` with its evidence rather than an exception. On a research task
over 157 incident reports, Ultron chose 99 of these frames on its own and finished 20% faster than stock Pi.

### 3. Long work never blocks the turn

A shell command or tool call still running after 30 seconds becomes a background job with a handle. When it
finishes, a short `<runtime_event>` message wakes the model. It never polls, sleeps or stares at a progress bar,
and a test suite can run while it fixes something else.

### 4. It has a runtime, not just a loop

- **Sub-agents with their own kernels** (`rlm.spawn`), typed agents with validated inputs and outputs
  (`agents.invoke`), and agent graphs with joins and bounded revision loops (`workflows.run`).
- **Agents as Python classes**: the docstring is the prompt, `...` methods are model-driven with typed returns
  checked by the host, and fields are durable state.
- **Code skills**: a procedure that worked is saved as Python with a test, and only goes live when the test passes.
  Versions roll back.
- **The model manages its own context** (`ctx.forget`, `ctx.summarize`, `ctx.pin`, `ctx.note`), and finished task
  results collapse to one line after the model has seen them.
- **Durable state**: a task journal, per-turn budgets for tokens, turns, wall time and cost, a memory cap over the
  kernel's whole process tree, and kernel snapshots that survive restarts.

### 5. Memory with judgement

Automatic memory through Hindsight is gated by Jev: before each turn
it decides whether memory is needed at all, and after each turn whether it is worth keeping. A low score means no
lookup, so unrelated memories never leak into answers.

### 6. You can see it think

- **The RLM graph** draws the run as a live tree: turn, cells, tool calls, sub-agents, `rlm.map` fan-outs with
  progress bars, workflows and jobs, with budget gauges and a kernel strip. Ctrl+R docks it and Alt+G opens it
  full screen, where you can step into any node and see its input, result and the Python call that fetches it.
- **Jev's presence**: a footer indicator that pulses when Jev decides, one-line notes in the transcript showing what
  memory was used and whether the turn was kept, and a `/jev` view with a decision timeline and threshold gauges.

## How it compares

| | Typical tool-calling agent | Ultron |
|---|---|---|
| Model's interface | a menu of tools, one call per step | one persistent Python REPL |
| Large files and outputs | pasted into context | handles; only printed slices come back |
| Many independent steps | many model turns | one cell with a loop or `asyncio.gather` |
| MCP servers | separate tools | Python functions in the REPL |
| Slow commands | the turn waits | background jobs, completion events |
| Reading lots of text | the model reads it all | bounded sub-model frames under a budget |
| Delegation | sub-agent tools, if any | sub-agents, typed agents, workflows, agent classes |

## Results so far

Measured with [`scripts/eval-quality.mjs`](scripts/eval-quality.mjs) against stock Pi on the same model. Every
result file is in [`acceptance/quality/`](acceptance/quality).

**glm-5.3-flash** (a small, fast model), thinking `max`:

| Task set | Pi | Ultron |
|---|---|---|
| Hard set: 15 tasks (multi-file bugs, refactors, large data, log forensics), 2 trials | 27/30 | **29/30**, a third fewer tool calls, same speed |
| Research pilot: find expired-certificate incidents among 157 reports, 1 trial | pass, 342 s | pass, **273 s**, 99 sub-model frames |
| Parallel work: a 150 s test suite plus two bug fixes, 2 trials | 2/2 | 2/2, same speed |

**gpt-6-sol** (a strong model), default thinking, 1 trial:

| Task set | Pi | Ultron |
|---|---|---|
| Hard set: 15 tasks | 14/15, median 31 s | **15/15**, median 45 s, 1.55x the cost |
| Hard set, 5 tasks after the cost fixes below | 5/5, 191k tokens, median 33 s | 5/5, 217k tokens, median 39 s |
| Research: 400 reports, 2.1 MB | exact, 71 s, 289k tokens | exact, **73 s, 287k tokens** (was 369 s, 8.0M) |
| Parallel work | pass, 161 s | pass, 161 s |

Ultron has been at least as accurate as Pi on every set, and ahead on the hard set with both models. The first
gpt-6-sol runs showed it was slower and far more expensive on the research task: it handed the corpus to 24 nested
sub-agents instead of searching first, spending 8.0M tokens. The runtime guide now says to search in code and read
the deciding passages before delegating, sub-agents can nest at most two levels, and `rlm.map` has a default token
budget. After those fixes Ultron matches Pi on the research task and is within about 1.1x of its tokens on the hard
tasks ([`docs/performance.md`](docs/performance.md) has the breakdown). These are small samples; treat them as
early evidence, not benchmarks. Behaviour is also covered by 56 acceptance rows (A01-A56) judged by a runner.

## Install

Requirements: **Node.js 22.19 or newer** and **Python 3** on your `PATH` (the REPL uses the system `python3`).

```bash
npm install -g --ignore-scripts https://github.com/alfredosdpiii/ultron/releases/latest/download/ultron.tgz
ultron --version
```

The same command updates an existing install. Nothing Ultron depends on needs lifecycle scripts, hence
`--ignore-scripts`. Settings and sessions live in `~/.ultron/agent`, separate from Pi's `~/.pi/agent`, so both
can be installed side by side.

## Quick start

```bash
cd your-project
ultron                                   # interactive TUI
ultron -p "why does test_parser fail?"   # one-shot print mode
ultron --mode rpc                        # Pi-compatible JSONL RPC
```

Pick a provider as in Pi: `/login` for subscription providers, or `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY` and the like. Custom OpenAI-compatible endpoints go in `~/.ultron/agent/models.json`.

Useful keys: Up/Down and Alt+R for prompt history, Ctrl+R for the RLM panel, Alt+G for the full-screen graph,
Alt+J for Jev, Ctrl+O to expand cells and help. `/hotkeys` lists them all.

## The REPL at a glance

| | |
|---|---|
| `await bash('''cmd''')`, `await edit(...)`, `await read(path)` | shell, exact edits, file text (a handle for large files) |
| `await view_image(path_or_figure)` | show the model a screenshot, diagram or matplotlib figure |
| `await mcp.call(tool, **args)`, `await tools.call(name, {...})` | MCP servers and any Pi extension tool |
| `await rlm.load(...)`, `rlm.infer(...)`, `rlm.map(...)` | handles and bounded sub-model frames |
| `await rlm.spawn(task)`, `agents.invoke(...)`, `workflows.run(...)` | sub-agents, typed agents, agent graphs |
| `@agent class ...` | agents as Python classes |
| `ctx.*`, `skills.propose_code(...)`, `memory.*` | context control, tested code skills, memory |
| `state` | a dict that survives kernel restarts |

Opt-outs: `ULTRON_TOOLS=native` gives the model Pi's `read`, `edit`, `write` and `bash` tools again, and
`ULTRON_EXTENSION_TOOLS=native` makes extension and MCP tools separate model tools again.

## Optional services

- **Hindsight** memory is used at `http://localhost:8888` when it is running (`ULTRON_HINDSIGHT_URL` to change,
  `off` to disable). Without it, memory calls fail quietly and turns are unaffected.
- **Jev** gates memory and needs `TYPESAFE_API_KEY`. Without it, automatic memory stays off.

## Safety

There is no sandbox. Model-written Python runs with your user's permissions, like Pi's `bash` tool. Resource limits
apply (the process-tree memory cap, CPU and wall-time budgets, per-turn token and turn limits), but they are not
isolation. Run Ultron in a container or VM if you need a boundary; Pi's
[containerization guide](packages/coding-agent/docs/containerization.md) applies.

## Where the ideas come from

Ultron combines ideas from several projects and papers:

- **Prime Intellect's RLM harness**: the REPL as the sole tool, shell and MCP as Python skills, output truncation.
- **NVIDIA's NOOA**: bounded previews instead of dumps, and agents as typed Python classes.
- **Autolith**: bounded inference over inputs the root never reads, with contracts, repair and budget trees.
- **LLM-as-Code**: context that collapses when work returns, and self-improvement committed as tested code.
- **Unreal Agent**: long work that never blocks the turn.
- **waku-agent**: memory behind a retrieval gate.

[`supremeplan.md`](supremeplan.md) records how each one landed and what was measured.

## Development

```bash
npm install --ignore-scripts
npm run build:offline           # build all packages without refreshing model data
npm run check                   # lint, format, type check
./test.sh                       # all tests
npm run test:acceptance         # acceptance rows A01-A56, judged by the runner
node scripts/pack-release.mjs   # build the self-contained release tarball (after a build)
```

Design and status: [`docs/implementation-status.md`](docs/implementation-status.md),
[`docs/ultron-architecture.md`](docs/ultron-architecture.md), [`supremeplan.md`](supremeplan.md).

## Credits and license

Ultron is built on [Pi](https://github.com/badlogic/pi-mono) by Mario Zechner and contributors, and keeps Pi's
MIT license (see [LICENSE](LICENSE)). Pi's documentation at [pi.dev](https://pi.dev) covers the interface,
providers, extensions and settings that Ultron shares.
