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
npm install -g ultron-agent
ultron setup
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
- **The RLM pane** (Alt+W on terminals 120 columns or wider) splits the screen: sub-agents and workflow nodes as
  boxes in dependency waves, with arrows to the nodes that depend on them, and cards showing each node's model, latest
  text, time, turns and tool calls. It opens by itself, without taking the focus, when a turn spawns children, runs a
  workflow or fans out an `rlm.map` (once per turn; close it with q and it stays closed until the next one; turn it off
  with the `rlmPaneAutoOpen` setting or `ULTRON_RLM_PANE_AUTO=off`). While nodes run, a wave summary sits above the input.
- **Jev's presence**: a footer indicator that pulses when Jev decides, one-line notes in the transcript showing what
  memory was used and whether the turn was kept, and a `/jev` view with a decision timeline and threshold gauges.

### 7. Reviews that check their own findings (`/review`)

`/review` is an optional command for reviewing a change: the working tree plus the current branch by default, or
`/review main`, `/review 123` (a PR, through `gh`), `/review src/`. Five specialist reviewers (correctness,
security, architecture, tests, AI/LLM integration) run as bounded `rlm.map` frames over the diff in chunks with the
surrounding code; then every finding goes to a verifier frame that reads the lines it cites and the callers of the
code involved. Rejected findings never reach the report, confirmations must quote the source, and the report ends
with counts, cost and what was not checked. One token cap covers it all (`--budget`, default 300k); `--only sec,bugs`,
`--model`, `--deep` (a sub-agent re-checks undecided findings) and `--post` (only after you say yes) adjust it.
It adds nothing to the prompt of ordinary turns. Details: [`docs/review.md`](docs/review.md).

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

**glm-5.3-flash** (a small, fast model), thinking `max`, 2 trials per task (release 0.87.12):

| Task set | Pi | Ultron |
|---|---|---|
| Hard set: 15 tasks (multi-file bugs, refactors, large data, log forensics) | 28/30, median 116 s, 2.53M tokens, 288 tool calls | 28/30, median 128 s, **1.96M** tokens, **182** tool calls |
| Research pilot: expired-certificate incidents among 157 reports | 1/2 (one run lost to a provider rate limit), 377 s | **2/2**, 397 s, 157 sub-model frames |
| Parallel work: a 150 s test suite plus two bug fixes | 2/2, median 175 s | 2/2, median 196 s |

On the small model the two agents are level on accuracy and speed (each was faster on about half the hard tasks);
Ultron uses about a quarter fewer tokens and a third fewer tool calls. Each agent lost one hard run to a single model
turn that ran past the 20-minute limit.

**gpt-6-sol** (a strong model), default thinking, 2 trials per task, both agents with an isolated home (release 0.87.10):

| Task set | Pi | Ultron |
|---|---|---|
| Hard set: 15 tasks (0.87.11) | 29/30, median 34 s, 696k tokens, $1.11 | **30/30**, median 36 s, **586k** tokens, $1.02 |
| Research: 400 reports, 2.1 MB | 2/2, median 76 s, 638k tokens | 2/2, median **73 s**, **372k** tokens |
| Parallel work | 2/2, median 162 s | 2/2, median 162 s |
| Delegation: 6 services, bugs readable from the code, 300 s budget | 2/2, 119 s and 145 s | 2/2, **113 s and 78 s**, 6 sub-agents each |
| Delegation, deep: 6 services, bugs only a slow harness reveals, 300 s budget (0.87.11) | 6/6 both times, but **over budget**: 355 s and 366 s, $0.47 avg | 6/6 both times, **within budget: 165 s and 176 s**, $0.63 avg, 6 sub-agents each |

The deep delegation task is where the runtime matters most: each service hides three bugs behind a 21-second test
harness that stops at the first failure, so working through six services one at a time cannot fit the budget.
Ultron split the work into one sub-agent per service on its own, without being told to, waited for them without
checking in, and finished in less than half of Pi's time for about 1.35x the cost. Pi fixed everything too, but
sequentially, and ran over the budget in both trials.

Ultron has been at least as accurate as Pi on every set so far, at about the same token cost on hard tasks and 40%
fewer tokens on research. Releases 0.87.10 and 0.87.11 closed the short-task speed gap (median 60 s, then 40 s, then 36 s against Pi's
34 s; the rest is run-to-run noise): the extra time was extra model turns caused by shell text mangled by Python string escapes, which the kernel
now passes to bash as written. Before release 0.87.6, the research task cost Ultron 8.0M tokens because it handed the
corpus to nested sub-agents instead of searching first ([`docs/performance.md`](docs/performance.md) has the
breakdown). Two trials is still a small sample; treat these as evidence, not benchmarks. Behaviour is also covered by
56 acceptance rows (A01-A56) judged by a runner.

## Install

Requirements: **Node.js 22.19 or newer** and **Python 3** on your `PATH` (the REPL uses the system `python3`).

```bash
npm install -g ultron-agent     # the package is ultron-agent; the command is ultron
ultron setup                    # guided setup: provider and model, Jev key, Hindsight memory
```

The same install command updates an existing install, and so does `ultron update`, which checks the npm registry for
a newer `ultron-agent` (set `ULTRON_SKIP_VERSION_CHECK=1` to turn off the startup check). No dependency runs an install
script, so npm 11's allow-scripts prompt does not apply. The experimental plugin packages (`-e` with the experimental
server) are the one feature that needs esbuild; install it next to ultron with `npm install -g esbuild` if you use them.

The same package is attached to every GitHub release, for installing without the npm registry:

```bash
npm install -g https://github.com/alfredosdpiii/ultron/releases/latest/download/ultron.tgz
```

Settings and sessions live in `~/.ultron/agent`, separate from Pi's `~/.pi/agent`, so both can be installed side
by side.

### `ultron setup`

A terminal wizard, also offered the first time `ultron` starts without a usable model. Every step can be skipped
with Esc, nothing is overwritten without asking, secrets are typed masked and saved readable only by you, and it
can be run again at any time.

1. **Environment**: checks Node.js and `python3`, with fixes when either is missing.
2. **Provider and model**: sign in with a subscription or save an API key (Pi's `/login`), or add a custom
   OpenAI-compatible endpoint to `models.json`; pick the default model and thinking level; then one tiny live
   request shows whether it works, with the provider's error if not.
3. **Jev API key**: saved to `~/.ultron/agent/jev-api-key`, optionally checked with one small request.
4. **Hindsight**: finds a running server, starts it with Docker (`ghcr.io/vectorize-io/hindsight`, asking for the
   LLM key Hindsight itself needs), saves a different URL, or shows the manual install.
5. **Loki guardrails**: shows the bundled Loki version and which analyzers it would use are missing (with install
   hints), and turns auto-install, auto-commit and advise-only mode on or off.
6. **Summary** of what changed and where it lives.

## Quick start

```bash
cd your-project
ultron                                   # interactive TUI
ultron -p "why does test_parser fail?"   # one-shot print mode
ultron --mode rpc                        # Pi-compatible JSONL RPC
```

`ultron setup` configures a provider. Pi's ways work too: `/login` for subscription providers, or
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` and the like. Custom OpenAI-compatible endpoints go in
`~/.ultron/agent/models.json`.

Useful keys: Up/Down and Alt+R for prompt history, Ctrl+R for the RLM panel, Alt+W for the RLM pane, Alt+G for the full-screen graph,
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

- **Hindsight** memory is used at `http://localhost:8888` when it is running. `ultron setup` can install it with
  Docker or save another address as the `hindsightUrl` setting; `ULTRON_HINDSIGHT_URL` overrides both, and `off`
  disables memory. Without it, memory calls fail quietly and turns are unaffected.
- **Jev** gates memory and needs a key, from `ultron setup` (`~/.ultron/agent/jev-api-key`) or `TYPESAFE_API_KEY`.
  Without it, automatic memory stays off.

## Loki guardrails

[Loki](https://github.com/alfredosdpiii/loki) checks every file Ultron writes with deterministic rules and real
analyzers (hardcoded secrets, XSS and injection sinks, protected files, net-new Ruff/mypy/tsc/Oxlint/Clippy/Credo
findings). It is bundled and on by default:

- `edit()` and `write()` are checked before the file changes; a finding raises `ValueError` and nothing is written.
  A check that takes longer than 5 s (`ULTRON_LOKI_TIMEOUT_MS`) lets the write through with a visible "Loki did not
  check this write" note.
- Files a cell changes any other way (`bash('sed -i ...')`, `Path.write_text`) are checked after the cell, in the
  background; findings arrive with the next cell result.
- In a Git repository without `.loki/`, Ultron creates only `.loki/` (engine and default policy) and commits just
  that directory as "Add Loki guardrails" with your Git identity, leaving your other staged and unstaged changes
  alone. It never pushes and never skips hooks; if a hook or signing refuses the commit, or a merge or rebase is in
  progress, HEAD is detached or `CI` is set, `.loki/` stays uncommitted and the session says why. **On a fork, that
  commit rides along in your pull requests**; drop it or turn auto-commit off. A repository's own `.loki/` is used
  as committed and never modified.
- Turn it off with `ULTRON_LOKI=off` (or `ULTRON_LOKI=advise` to report without blocking), stop the auto-install or
  auto-commit with `ULTRON_LOKI_AUTOINIT=off` / `ULTRON_LOKI_AUTOCOMMIT=off`, or use `ultron setup` (the global
  `loki` setting, which also takes `ignoreRepos`). Loki needs `python3` 3.11 or newer; without it the session says
  so and runs unchecked.

`npm run loki:update -- <loki-checkout | version>` refreshes the bundled engine, pinned by sha256.

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
npm run publish:npm -- --dry-run   # build, pack and check the ultron-agent npm package (drop --dry-run to publish)
```

Design and status: [`docs/implementation-status.md`](docs/implementation-status.md),
[`docs/ultron-architecture.md`](docs/ultron-architecture.md), [`supremeplan.md`](supremeplan.md).

## Credits and license

Ultron is built on [Pi](https://github.com/badlogic/pi-mono) by Mario Zechner and contributors, and keeps Pi's
MIT license (see [LICENSE](LICENSE)). It bundles [Loki](https://github.com/alfredosdpiii/loki) (MIT, its license in
`packages/coding-agent/src/ultron/loki-engine/LICENSE`). Pi's documentation at [pi.dev](https://pi.dev) covers the interface,
providers, extensions and settings that Ultron shares.
