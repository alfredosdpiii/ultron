# Ultron

Ultron is a terminal coding agent whose only built-in tool is a persistent Python REPL. The model reads files,
runs commands, edits code, processes data and delegates to sub-agents by writing Python, instead of choosing from a
menu of tools. It is a fork of [Pi](https://github.com/badlogic/pi-mono) and keeps Pi's interface: the same TUI,
`-p` print mode, `--mode json`, and `--mode rpc`, the same providers, extensions, skills and prompt templates.

The design takes ideas from Prime Intellect's RLM harness (the REPL as the sole tool), NVIDIA's NOOA (bounded
previews and typed agent objects), Autolith (bounded inference over large inputs), LLM-as-Code (context that
collapses when work returns), Unreal Agent (long work that never blocks the turn) and waku-agent (gated memory).
[`supremeplan.md`](supremeplan.md) describes how each one landed.

## Install

Requirements: **Node.js 22.19 or newer** and **Python 3** on your `PATH` (the REPL runs the system `python3`).

```bash
npm install -g --ignore-scripts https://github.com/alfredosdpiii/ultron/releases/latest/download/ultron.tgz
ultron --version
```

The same command updates an existing install to the latest release. `--ignore-scripts` is intended: nothing Ultron depends on needs lifecycle scripts. Ultron keeps its settings and
sessions in `~/.ultron/agent`, separate from Pi's `~/.pi/agent`, so both can be installed side by side.

## Quick start

```bash
cd your-project
ultron                                   # interactive TUI; /login or an API key env var to pick a provider
ultron -p "why does test_parser fail?"   # one-shot print mode
ultron --mode rpc                        # Pi-compatible JSONL RPC
```

Providers work as in Pi: `/login` for subscription providers, or the usual environment variables
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, ...). Custom OpenAI-compatible endpoints go in
`~/.ultron/agent/models.json` with the same format as Pi's `models.json`.

## What the model gets

One tool, `rlm`: a Python cell that runs in a kernel that persists for the whole session. Pre-imported, all async:

| | |
|---|---|
| `await bash('''cmd''')` | shell in the working directory; slow commands turn into background jobs after 30 s |
| `await edit(path, old_str, new_str)` | exact single-occurrence edits that fail loudly when stale or ambiguous |
| `read(path)` | text for normal files, a handle for large ones |
| `h = await rlm.load(path)` | a handle to a large input the model never reads whole (`h.search`, `h.lines`, `h.chunks`) |
| `await rlm.infer(task, context=[...], contract=...)` / `rlm.map(...)` | bounded sub-model calls over explicit slices, validated against a JSON-schema contract, under a shared budget |
| `await rlm.spawn(task)`, `agents.invoke(...)`, `workflows.run(...)` | sub-agents, typed agents and agent graphs |
| `@agent class ...` | agents defined as Python classes: docstring is the prompt, `...` methods are model-driven, fields are durable state |
| `ctx.history / forget / summarize / pin / note` | the model manages its own context |
| `skills.propose_code(...)` | procedures saved as tested Python skills, activated only when their tests pass |
| `memory.prepare / propose` | long-term memory through Hindsight, gated by Jev (optional) |

Output over about 20 KB is cut in the middle and large values are shown by reference, so data stays in the kernel.
When background work finishes, a short `<runtime_event>` message wakes the model instead of making it poll.
Set `ULTRON_TOOLS=native` to give the model Pi's `read`, `edit`, `write` and `bash` tools again.

The `/rlm` panel (Ctrl+R) shows the task tree, running kernels, jobs and sub-model frames; `/jev` (Alt+J) shows
memory and routing decisions.

## Optional services

- **Hindsight** memory: Ultron uses `http://localhost:8888` when it is running (`ULTRON_HINDSIGHT_URL` to change,
  `off` to disable). Without it, memory calls fail quietly and turns are unaffected.
- **Jev** decides whether a turn needs memory and whether to keep it; it needs `TYPESAFE_API_KEY`. Without Jev,
  automatic memory stays off.

## Results so far

Measured with [`scripts/eval-quality.mjs`](scripts/eval-quality.mjs) against stock Pi on the same model
(glm-5.3-flash, thinking `max`); every result file is in [`acceptance/quality/`](acceptance/quality).

| Task set | Pi | Ultron |
|---|---|---|
| Hard set: 15 tasks (multi-file bugs, refactors, large data, log forensics), 2 trials | 27/30 | **29/30**, a third fewer tool calls, same speed |
| Research pilot: find expired-certificate incidents among 157 reports (1 trial) | pass, 342 s | pass, **273 s**, 99 sub-model frames chosen unprompted |
| Parallel work: 150 s test suite plus two bug fixes (2 trials) | 2/2 | 2/2, same speed |

These are small samples on one model; treat them as early evidence, not benchmarks.

## Safety

There is no sandbox. Model-written Python runs with your user's permissions, like Pi's `bash` tool. Resource limits
apply (a memory cap over the kernel's whole process tree, CPU and wall-time budgets, per-turn token and turn limits),
but they are not isolation. Run Ultron in a container or VM if you need a boundary; Pi's
[containerization guide](packages/coding-agent/docs/containerization.md) applies.

## Development

```bash
npm install --ignore-scripts
npm run build:offline      # build all packages without refreshing model data
npm run check              # lint, format, type check
./test.sh                  # all tests
npm run test:acceptance    # acceptance rows A01-A56, judged by the runner
node scripts/pack-release.mjs   # build the self-contained release tarball (after a build)
```

Design and status: [`docs/implementation-status.md`](docs/implementation-status.md),
[`docs/ultron-architecture.md`](docs/ultron-architecture.md), [`supremeplan.md`](supremeplan.md).

## Credits and license

Ultron is built on [Pi](https://github.com/badlogic/pi-mono) by Mario Zechner and contributors, and keeps Pi's
MIT license (see [LICENSE](LICENSE)). Pi's own documentation at [pi.dev](https://pi.dev) covers the interface,
providers, extensions and settings that Ultron shares.
