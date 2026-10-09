# Ultron

**A terminal coding agent that programs its way through your work instead of picking tools from a menu.**

Most coding agents give the model a list of tools (read a file, run a command, edit, search) and let it call
them one at a time, pasting every result into its context. Ultron gives the model one thing: a persistent Python
REPL. Shell commands, edits, file reads, your MCP servers, sub-agents and sub-model calls are all Python functions
inside it. The model writes a cell that loops, filters, fans out and keeps the results in variables, and only what
it prints comes back into its context.

It is a fork of [Pi](https://github.com/badlogic/pi-mono) and keeps Pi's interface, so the TUI, `-p` print mode,
`--mode json`, `--mode rpc`, providers, extensions, skills and prompt templates all work the same. It can also run
with [Claude Code](https://claude.com/claude-code) as the model underneath, or inside Claude Code as its only tool.

```bash
npm install -g ultron-agent
ultron setup
```

![Native ultron: a cell reads a CSV, one rlm.map tags every row, and the RLM pane opens beside the chat](docs/demos/native.gif)

*Native `ultron` on glm-5.3-flash. The first cell fails and the next one fixes it, then one `rlm.map` runs 8
sub-model frames. The RLM pane opens by itself when the fan-out starts, and Loki set itself up on the first turn.*

## Which mode should I use?

Ultron runs in three ways. All three use the same runtime: the REPL, handles, frames, sub-agents, jobs, Loki and
budgets. What differs is whose interface you see and which model drives the root agent.

| | `ultron` | `ultron --claude` | `ultron claude` |
|---|---|---|---|
| You see | Ultron's TUI | Ultron's TUI | Claude Code's TUI, plus `ultron watch` for the RLM view |
| Root model | any provider: API key, subscription login, or a custom OpenAI-compatible endpoint | Claude Code on your login, `claude-opus-5-5` by default | Claude Code on your login, `claude-opus-5-5` by default |
| Who manages the context | Ultron (`ctx.*`, compaction, results that collapse to one line) | Claude Code | Claude Code |
| A finished job or sub-agent wakes the model | yes | yes | no: the event arrives with the next `rlm` result or your next message |
| Ultron's panels and commands (`/review`, `/settings`, `/rlm`) | yes | yes | no (`ultron watch` shows the RLM view) |
| Use it when | you use a non-Claude model, or want every Ultron feature | you want Claude on your Pro or Max plan in Ultron's UI | you prefer Claude Code's own UI and keybindings |

The [Claude Code](#claude-code) section has the details of both Claude modes, and of using Claude Code only for
sub-model frames.

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
Frames can run on a different, cheaper model than the root (see [Models](#models-settings--models)).

### 3. Long work never blocks the turn

A shell command or tool call still running after 30 seconds becomes a background job with a handle. When it
finishes, a short `<runtime_event>` message wakes the model. It never polls, sleeps or stares at a progress bar,
and a test suite can run while it fixes something else.

### 4. It has a runtime, not just a loop

- **Sub-agents with their own kernels** (`rlm.spawn`), typed agents with validated inputs and outputs
  (`agents.invoke`), and agent graphs with joins and bounded revision loops (`workflows.run`).
- **Checked sub-agent verdicts.** A sub-agent ends with `rlm.finish(status, summary, evidence=..., changed_files=...)`.
  `passed` needs concrete evidence (a test command and its result, or file lines), and a malformed verdict is sent
  back to be fixed. The host snapshots the workspace before and after the child runs. If a file the child says it
  changed did not change, the verdict is `contradicted`, and a workflow node with a contradicted verdict fails. A
  child that ends without a verdict keeps its result but is marked unverified.
- **Nesting is opt-in.** A sub-agent cannot spawn its own sub-agents unless it was started with `depth=N`, and no
  chain goes deeper than 3 levels (`ULTRON_SPAWN_DEPTH`). The limit also holds across Claude Code child processes.
- **Agents as Python classes**: the docstring is the prompt, `...` methods are model-driven with typed returns
  checked by the host, and fields are durable state.
- **Code skills**: a procedure that worked is saved as Python with a test, and only goes live when the test passes.
  Versions roll back.
- **The model manages its own context** (`ctx.forget`, `ctx.summarize`, `ctx.pin`, `ctx.note`), and finished task
  results collapse to one line after the model has seen them.
- **Durable state**: a task journal, per-turn budgets for tokens, turns, wall time and cost, a memory cap over the
  kernel's whole process tree, and kernel snapshots that survive restarts.
- **A kernel that stays up.** The kernel speaks its protocol on private pipes (fds 3 and 4). Anything a cell writes
  straight to stdout or stderr (`os.write`, subprocesses, C extensions) becomes cell output instead of breaking the
  connection. There is no lifetime output cap any more: an earlier 4 MiB cap per kernel restarted the kernel after
  about 85 large cells and lost its variables. Now each cell's stdout and stderr are capped at 4 MiB each.

#### Sub-agents in their own Git worktrees

Children that edit files at the same time can overwrite each other's work in a shared checkout.
`rlm.spawn(brief, name=..., worktree=True)` gives a child a private Git worktree instead. It is branched from the
parent's tree as it is at that moment, uncommitted changes included, on `ultron/<session>/<name>`. It lives under
`.git/ultron-worktrees/`, so the parent's tools and `git status` never see it. The parent's index and files are not
touched.

- The child's kernel, `bash`, `edit`/`write`/`read`, jobs, Loki checks and verdict check all run in the worktree.
  An absolute path into the parent's checkout (as briefs often give them) is mapped to the worktree's copy. Files
  the child still changes in the parent's tree show up as `check["outside"]`.
- When the child ends, its changes are committed on its branch. The commit message is its verdict's summary, with
  no hooks, no signing and no trailers. Its result gains
  `worktree: {branch, path, commit, changed_files, diffstat}`. A child that changed nothing has its worktree and
  branch removed right away.
- `await rlm.merge(hs)` merges each child's branch into the parent's working tree in order, three-way, against a
  snapshot of the tree as it is now. The changes land as uncommitted edits (new files untracked). Nothing is
  committed on your branch: you review and commit. The first conflict stops the merge and writes nothing of that
  child: the report names the files and hunks, and the tree stays as the earlier merges left it.
  `on_conflict="skip"` goes on with the next child, and `"markers"` writes conflict markers for you to resolve.
  Children that did not pass are skipped unless `include_failed=True`. Merged and empty children's worktrees and
  branches are removed; conflicted or failed ones are kept for inspection.
- Group before spawning: put coupled tasks (the same function, or one building on another) in one child's brief,
  and give independent tasks their own children. Every worktree child gets its own worktree; worktrees are never
  shared. A child's own children branch from its worktree and merge back into it. `workflows.run` nodes take
  `worktree: true`, and Claude Code children (`ultron --claude`, `ultron claude`) run in their worktree too.
- Git ignores the parent's `node_modules`, `.venv` and `.env`, so a new worktree would not have them. Gitignored
  `node_modules`, `.venv` and `venv` directories are linked in by default (a `node_modules` entry by entry, so
  workspace packages resolve to the worktree's own sources), and small `.env` files are copied.
  `worktree_setup={"link": [...], "copy": [...], "command": "npm ci"}`, or the `worktrees` setting (`link`, `copy`,
  `setupCommand`), changes this. Linked dependencies are shared, so children must not install different ones at the
  same time. A package installed in editable mode (`pip install -e .`) still imports the parent's sources, and
  dev servers or databases that children start share the parent's ports.
- Cleanup: when a session ends, its finished children's worktrees are removed and their unmerged branches are kept.
  Worktrees of a crashed session are pruned when the next session starts in the repository. Uncommitted work is
  committed to the branch first, and only Ultron's own worktrees and branches are touched.
  `await rlm.worktrees.list()` and `await rlm.worktrees.cleanup(branches=False)` manage them by hand, and
  `ULTRON_KEEP_WORKTREES=1` keeps everything.
- Limits: this needs Git and a repository with at least one commit. `worktree="auto"` falls back to the shared
  tree when either is missing. Submodules are not initialized in a worktree, and Git LFS files are checked out only
  if git-lfs is installed.

### 5. Memory on purpose

Long-term memory lives in Hindsight and the model uses it from the REPL: `memory.prepare(query)` recalls,
`memory.propose(text, evidence)` keeps a fact, `memory.correct` and `memory.forget` fix it. Nothing recalls or keeps
on its own, so unrelated memories never leak into answers, and a write that holds a secret or credential is refused
before it reaches Hindsight.

### 6. You can see it think

- **The RLM graph** draws the run as a live tree: turn, cells, tool calls, sub-agents, `rlm.map` fan-outs with
  progress bars, workflows and jobs, with budget gauges and a kernel strip. Ctrl+R docks it and Alt+G opens it
  full screen, where you can step into any node and see its input, result and the Python call that fetches it.
- **The RLM pane** (Alt+W on terminals 120 columns or wider) splits the screen: sub-agents and workflow nodes as
  boxes in dependency waves, with arrows to the nodes that depend on them, and cards showing each node's model, latest
  text, time, turns and tool calls. It opens by itself, without taking the focus, when a turn spawns children, runs a
  workflow or fans out an `rlm.map` (once per turn; close it with q and it stays closed until the next one; turn it off
  with the `rlmPaneAutoOpen` setting or `ULTRON_RLM_PANE_AUTO=off`). While nodes run, a wave summary sits above the input.
- **The session report** (`/usage`, `ultron usage`) answers "did this session use depth, or only the root?" without
  a script over the session file: turns and wall time, cells and the REPL APIs they named, frames and sub-agents
  (by model, with verdict checks, worktree branches and merges), tokens and cost per lane kind and per model, what the
  guardrails did (Loki, masked secrets, hints, steers, refused work) and the memory store's operations. `/usage` reports the
  running session; `ultron usage [session-id|path]` reads any session file offline (no model call, no server, nothing
  written), `--last N` prints one line per recent session, and `--json` is the same report for scripts
  (`ultron.session-report/1`). It works for `ultron`, `ultron --claude` and `ultron claude` sessions; a number a
  session did not keep is printed as `not recorded`, and a cost nobody reported as `unknown`, never as zero.

  ```text
  $ ultron usage
  Session 01a0fae6-d1e8-716d-b0c4-c136bcf29a2a  [ultron]
  ~/work/demo  ·  2026-10-02 08:37 to 2026-10-02 08:37  ·  348.8 kB

  Depth      depth 2: 4 frames, 4 sub-agents (1 nested)
  Turns      1 turn, 1s wall
  Root       scripted/scripted (4 responses)
  Cells      6 cells, 1 failed
             bash 1 · write 1 · rlm.infer 1 · rlm.map 1 · rlm.spawn 2 · rlm.collect 2 · rlm.merge 1
             root 3 · sub-agents 3
  Frames     4: 4 complete · 906 tokens
             from 1 rlm.map call and 1 rlm.infer call
             scripted/scripted  4 frames  906 tokens
  Sub-agents 4: 4 completed · max depth 2 (1 nested)
             verdicts: 2 verified · 0 contradicted · 2 unverified (2 without a verdict)
             scripted/scripted  4 sub-agents  18.7k tokens
             worktree ultron/01a0fae6/writer: 1 file, merge merged
  Workflows  0 runs
  Other work 0 typed-agent tasks · 0 background jobs

  Tokens and cost
    lane        responses  input  output  cache read  cache write  total  cost
    root                4  11.1k      32           0            0  11.1k  unknown
    frames              4    874      32           0            0    906  unknown
    sub-agents          7  18.7k      56           0            0  18.7k  unknown
    total              15  30.6k     120           0            0  30.7k  unknown
    model              responses  input  output  cache read  cache write  total  cost
    scripted/scripted         15  30.6k     120           0            0  30.7k  unknown
    unknown: 15 responses used tokens and reported no price.

  Guardrails
    Loki     no write was checked
    Secrets  0 masked
    Hints    stuck-loop 0
    Nudges   tool rounds 0 · wait 0 · skill 0
    Limits   0 usage-limit blocks

  Memory
    Store    not recorded (no memory operation recorded: Hindsight not configured, or memory unused)

  $ ultron usage --last 3
  LAST ACTIVE       ID             CWD    MODE           MODEL           TURNS  CELLS  FRAMES  SUBS    TOKENS  COST                           DEPTH
  2026-09-01 15:00  cccccccc-0003  ~/lib  ultron         priced/model-a      1      4       1     6     14.9k  $0.560                         depth 2: 1 frame, 6 sub-agents (1 nested), 1 typed-agent task, 1 background job
  2026-09-01 14:00  bbbbbbbb-0002  ~/app  ultron         priced/model-a      1      1       4     0      1.9k  $0.030 + unknown (3 unpriced)  depth 1: 4 frames, 0 sub-agents
  2026-09-01 13:00  eeeeeeee-0005  ~/app  ultron claude  Claude Code       n/r    n/r       1     2  11.2k +?  $0.610 (sub)                   depth 1: 1 frame, 2 sub-agents
  n/r: not recorded by that session (never zero). Tokens and cost of an `ultron claude` session leave out the root, which is Claude Code's.
  ```

  The first report is a scripted test session (its model has no price); the table is the test fixtures'. Under
  `ultron claude` the root's transcript, tokens and cost are Claude Code's, so Ultron counts that session's turns and
  cells itself; a session written before it did reports them as `n/r`.
- **A TUI that keeps up with long sessions.** Streaming a token, polling the RLM state or ticking a spinner used to
  lay out the whole transcript again. In a 400-entry session that took over 300 ms per event, with 1.1 to 1.5 s
  stalls on every idle poll. Now only the part that changed is redrawn, and a poll redraws only when its data changed.
  Scrolling uses the terminal's scroll region, so a one-line scroll writes about 1 KB instead of the whole screen
  ([render cost tests](packages/tui/test/render-cost.test.ts)).

### 7. Reviews that check their own findings (`/review`)

`/review` is an optional command for reviewing a change: the working tree plus the current branch by default, or
`/review main`, `/review 123` (a PR, through `gh`), `/review src/`. Five specialist reviewers (correctness,
security, architecture, tests, AI/LLM integration) run as bounded `rlm.map` frames over the diff in chunks with the
surrounding code; then every finding goes to a verifier frame that reads the lines it cites and the callers of the
code involved. Rejected findings never reach the report, confirmations must quote the source, and the report ends
with counts, cost and what was not checked. One token cap covers it all (`--budget`, default 300k); `--only sec,bugs`,
`--model`, `--deep` (a sub-agent re-checks undecided findings) and `--post` (only after you say yes) adjust it.
It adds nothing to the prompt of ordinary turns. Details: [`docs/review.md`](docs/review.md).

![/review --only bugs finds a percent coupon that is not divided by 100, and the verifier confirms it](docs/demos/review.gif)

*`/review --only bugs` on an uncommitted change with a planted bug (a percent coupon that is not divided by 100): one
finder frame and one verifier frame, 2,319 tokens in all, on glm-5.3-flash.*

### 8. Guardrails on by default

- **[Loki](#loki-guardrails)** checks every file Ultron writes (hardcoded secrets, injection sinks, protected files,
  new linter and type-checker findings) and blocks a bad write before it happens.
- **Secret masking.** Cell output is scanned before it reaches the model or the transcript. API keys, tokens,
  private keys, credentials in URLs and similar values become `[REDACTED:<kind>]`; the values in the kernel are left
  as they are. `ULTRON_MASK_SECRETS=off` turns it off. The same rules
  ([`secret-patterns.json`](packages/coding-agent/src/ultron/rlm/secret-patterns.json)) drive `npm run scan:secrets`,
  which checks this repository in CI and every release package before it is packed.
- **A stuck-loop detector** watches for three failing cells in a row, the same two cells alternating, or eight
  failures in one turn. It does not stop the run. The first time, it asks the model for three hypotheses and one cell
  that tests the likeliest; the second time, it tells the model to stop repeating the approach and say what blocks it.

## How it compares

| | Typical tool-calling agent | Ultron |
|---|---|---|
| Model's interface | a menu of tools, one call per step | one persistent Python REPL |
| Large files and outputs | pasted into context | handles; only printed slices come back |
| Many independent steps | many model turns | one cell with a loop or `asyncio.gather` |
| MCP servers | separate tools | Python functions in the REPL |
| Slow commands | the turn waits | background jobs, completion events |
| Reading lots of text | the model reads it all | bounded sub-model frames under a budget |
| Delegation | sub-agent tools, if any | sub-agents with checked verdicts, typed agents, workflows, agent classes |

## Results so far

Every result file is in [`acceptance/quality/`](acceptance/quality). All of these are small samples (one or two
trials per task), so read them as indicative.

### SWE-bench Verified

A seeded 50-task sample of [SWE-bench Verified](https://www.swebench.com/), one run per task, every agent on
`gpt-6.1-sol` at medium reasoning and given only the issue text, working inside the task's official container and
scored by the official evaluation (release 0.87.22, harness in [`evals/swebench/`](evals/swebench),
[results](acceptance/quality/2026-10-03-swebench-verified-50-v0.87.22-cliproxyapi_gpt-6.1-sol.md)):

| | Resolved | Wall time, 50 tasks | Tokens | Notional cost |
|---|---|---|---|---|
| **Ultron** | **44/50 (88%)** | **72 min** | **2.69M** | **$2.37** |
| Codex CLI 0.152.1 | 44/50 (88%) | 100 min | 9.43M | $4.33 |
| Pi 0.84.4 | 43/50 (86%) | 83 min | 3.16M | $2.84 |

Accuracy is a tie: a one-task gap at n=50 is noise, and in an earlier 10-task run of the same sample the single
differing task went the other way. What separates the agents is cost: Ultron resolved as many tasks as Codex with
29% of its tokens and 28% less time. OpenAI publishes no SWE-bench Verified score for this model to compare with, and
Codex ran without a catalogue entry for it, which may have cost it something. Ultron used no frames or sub-agents on
these tasks; they are single-file fixes.

The same first 10 tasks on Claude Opus 5.5 through Claude Code
([results](acceptance/quality/2026-10-03-swebench-verified-pilot10-claude-opus-5-5.md)): `ultron --claude` and plain
Claude Code each resolved 9/10 with the same verdict on every task, Ultron with 27% of the tokens (328k against
1.21M) and 47% of the notional cost.

### Against Pi

Measured with [`scripts/eval-quality.mjs`](scripts/eval-quality.mjs) against stock Pi on the same model.

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
| Delegation, tickets: 12 small tickets in shared files, one coupled pair, 300 s budget (1 trial, worktree build) | 10/12 (the coupled pair failed), 85 s, $0.04 | **12/12**, 148 s, $0.07, no sub-agents |

The deep delegation task is where the runtime matters most: each service hides three bugs behind a 21-second test
harness that stops at the first failure, so working through six services one at a time cannot fit the budget.
Ultron split the work into one sub-agent per service on its own, without being told to, waited for them without
checking in, and finished in less than half of Pi's time for about 1.35x the cost. Pi fixed everything too, but
sequentially, and ran over the budget in both trials. On the tickets task gpt-6-sol read all twelve tickets and the code in
three turns and made every edit in one cell, so it never needed sub-agents or worktrees (the self-check shows why
they matter when it does: children writing whole files in one checkout lose half the tickets).

Ultron has been at least as accurate as Pi on every set so far, at about the same token cost on hard tasks and 40%
fewer tokens on research. Releases 0.87.10 and 0.87.11 closed the short-task speed gap (median 60 s, then 40 s, then 36 s against Pi's
34 s; the rest is run-to-run noise): the extra time was extra model turns caused by shell text mangled by Python string escapes, which the kernel
now passes to bash as written. Before release 0.87.6, the research task cost Ultron 8.0M tokens because it handed the
corpus to nested sub-agents instead of searching first ([`docs/performance.md`](docs/performance.md) has the
breakdown). Behaviour is also covered by 56 acceptance rows (A01-A56) judged by a runner.

### With Claude Code underneath

Early evidence from one trial each. Treat it as indicative only.

**`ultron --claude` against vanilla Claude Code**, both on Claude Opus 5.5, on the 8-task fast check of the
[AI Workflow Benchmark](https://github.com/alfredosdpiii/ai-workflow-benchmark) (release 0.87.17,
[`2026-09-30-awb-fast-check-claude-opus-5-5.json`](acceptance/quality/2026-09-30-awb-fast-check-claude-opus-5-5.json)):

| | Claude Code | `ultron --claude` |
|---|---|---|
| Mean score | 78.1 | **93.8** |
| Tasks passed | 3/8 | **4/8** |
| Wall time, all 8 tasks | 747 s | **566 s** |
| Notional cost reported by Claude Code | $3.53 | **$2.28** |

AWB's Workflow Lift is +15.6 points: 2 tasks better, none worse, 6 tied. That is not significant at n=8 (p=0.50).
The two tasks that moved are a bug fix (0 to 100) and a refactor (75 to 100). AWB itself calls a fast check an
exploratory sample, not an estimate of full-suite performance.

**`ultron claude` against plain Claude Code**, both on Sonnet (`acceptance/quality/2026-09-29-*-claude-code_sonnet.json`):

| | Claude Code | `ultron claude` |
|---|---|---|
| Hard tasks (3) | 3/3, median 16 s, 189k tokens | 3/3, median 18 s, **60k** tokens |
| Research pilot (25 incidents to find) | precision and recall 1.0, 26 s, 268k tokens | precision and recall 1.0, 31 s, **93k** tokens |

Same accuracy for about a third of the tokens. Most of the difference is the system prompt, which is sent with every
request: Ultron's guide is about 3.9k tokens against Claude Code's 19k. Neither run used a frame or a sub-agent.

## Install

Requirements: **Node.js 22.19 or newer** and **Python 3** on your `PATH` (the REPL uses the system `python3`).
The Claude modes also need the [Claude Code](https://claude.com/claude-code) CLI, installed and logged in.

```bash
npm install -g ultron-agent     # the package is ultron-agent; the command is ultron
ultron setup                    # guided setup: provider and model, Hindsight memory, Loki
```

`ultron update` updates an existing install: it asks the npm registry for the latest `ultron-agent` and installs
that version with the package manager that installed Ultron. Running the install command again does the same.
Ultron also checks for a newer version at startup; set `ULTRON_SKIP_VERSION_CHECK=1` to turn that off. No dependency
runs an install script, so npm 11's allow-scripts prompt does not apply. The experimental plugin packages (`-e` with
the experimental server) are the one feature that needs esbuild; install it next to ultron with
`npm install -g esbuild` if you use them.

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
3. **Hindsight**: finds a running server, starts it with Docker (`ghcr.io/vectorize-io/hindsight`, asking for the
   LLM key Hindsight itself needs), saves a different URL, or shows the manual install.
4. **Loki guardrails**: shows the bundled Loki version and which analyzers it would use are missing (with install
   hints), and turns auto-install, auto-commit and advise-only mode on or off.
5. **Summary** of what changed and where it lives.

## Quick start

```bash
cd your-project
ultron                                   # interactive TUI
ultron -p "why does test_parser fail?"   # one-shot print mode
ultron --claude                          # the same TUI, with Claude Code (Opus 5.5) as the model
ultron claude                            # Claude Code's own TUI, with Ultron's REPL as its only tool
ultron --mode rpc                        # Pi-compatible JSONL RPC
ultron usage --last 5                    # what recent sessions did: depth, cells, tokens, cost
```

`ultron setup` configures a provider. Pi's ways work too: `/login` for subscription providers, or
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` and the like. Custom OpenAI-compatible endpoints go in
`~/.ultron/agent/models.json`.

### Signing in from Docker or over SSH

A subscription login in the browser (Claude Pro/Max, ChatGPT, OpenRouter, Radius) ends with the browser being sent
to a callback server that Ultron runs on `localhost` (ports 53692, 1455, a free port and 1456). When Ultron runs in
a Docker container or over SSH, the browser on your computer cannot reach that server and ends on "This site can't
be reached: localhost refused to connect". On macOS and Windows that is also the case with
`docker run --network host`, which joins the network of Docker's VM, not your computer's (unless Docker Desktop's
host networking is enabled). The login has not failed at that point:

- **Paste the address of that page.** Copy the failed page's full address from the address bar
  (`http://localhost:1455/auth/callback?code=…&state=…`) and paste it into the login prompt in the terminal, which
  is still waiting. The login dialog says the same.
- **Or publish the callback port** and let the callback server listen on the container's interface instead of
  loopback only, which a published port cannot reach:
  `docker run -p 127.0.0.1:1455:1455 -p 127.0.0.1:53692:53692 -e ULTRON_OAUTH_CALLBACK_HOST=0.0.0.0 …` (ChatGPT
  and Claude; without `--network host`).
- **Or sign in without a callback:** a device-code login (GitHub Copilot, xAI, Kimi, Meta, and "Device code login"
  for ChatGPT and Radius), an API key (`docker run -e ANTHROPIC_API_KEY …`, or "Use an API key" in `ultron setup`),
  or an OpenAI-compatible proxy on the host such as CLIProxyAPI, added as a custom endpoint in `ultron setup`.

Useful keys: Up/Down and Alt+R for prompt history, Ctrl+R for the RLM panel, Alt+W for the RLM pane, Alt+G for the full-screen graph,
Ctrl+O to expand cells and help. `/hotkeys` lists them all, and `/usage` reports what the session did.

### Running several sessions

Any number of `ultron` processes can run side by side, TUIs and `-p`/`--mode json`/`--mode rpc` runs alike, even
with different models, flags or environment (`ULTRON_LOKI=off`, `ULTRON_RLM_FRAME_MODEL=…`). Each process runs its own
server with its own settings; starting one never disconnects another or stops its turns and background jobs. A Session
lives in one process at a time: opening a Session that another `ultron` still has open fails with "is open in another
Ultron process" instead of taking it over.

After an upgrade, sessions started by the old version keep running under the old version until they end; new
processes run beside them. If a server or Session worker is lost anyway (a crash, a kill), the TUI restarts the worker
once on its own, and otherwise says so: the Session file is saved, so `ultron -c` resumes it. Background jobs that
were running in the lost worker are gone.

### Models (`/settings → Models`)

Every model Ultron uses can be picked in the TUI while a session runs. `/settings → Models` has one row per role, each
with a picker over the models your providers offer:

- **Session model**: the root agent's model. Changing it switches the running session and saves it as the default,
  as `/model` does.
- **Frame model** (`rlm.frameModel`): code-free `rlm.infer` and `rlm.map` frames that name no `model=`.
- **Review model** (`review.model`): `/review` frames. By default it follows the frame model.
- **Sub-agent model** (`rlm.childModel`): `rlm.spawn` sub-agents. Only tool-capable models are offered.
- **Frame thinking** (`rlm.frameThinking`): off, low, medium or high.
- **Claude Code mode**: the root, frame and sub-agent models of `ultron claude`, and the model of its Ultron
  sub-agents.

The first five apply to the running session: the next frame, review or sub-agent uses the new value. The Claude Code
mode rows apply the next time `ultron claude` starts. When an environment variable (for example
`ULTRON_RLM_FRAME_MODEL`) sets a role, its row says so and stays locked until you unset it. See
[settings](packages/coding-agent/docs/settings.md#models-of-rlm-work-ultron).

![/settings, Models: the frame model is switched to Claude Haiku through Claude Code, and frame thinking to low](docs/demos/settings.gif)

## The REPL at a glance

| | |
|---|---|
| `await bash('''cmd''')`, `await edit(...)`, `await read(path)` | shell, exact edits, file text (a handle for large files) |
| `await view_image(path_or_figure)` | show the model a screenshot, diagram or matplotlib figure |
| `await mcp.call(tool, **args)`, `await tools.call(name, {...})` | MCP servers and any Pi extension tool |
| `await rlm.load(...)`, `rlm.infer(...)`, `rlm.map(...)` | handles and bounded sub-model frames |
| `await rlm.spawn(task, depth=0)`, `agents.invoke(...)`, `workflows.run(...)` | sub-agents, typed agents, agent graphs |
| `rlm.spawn(task, name=..., worktree=True)`, `await rlm.merge(hs)` | a sub-agent in its own Git worktree, and merging its work back |
| `rlm.finish(status, summary, evidence=...)` | a sub-agent's checked verdict |
| `@agent class ...` | agents as Python classes |
| `ctx.*`, `skills.propose_code(...)`, `memory.*` | context control, tested code skills, memory |
| `state` | a dict that survives kernel restarts |

Opt-outs: `ULTRON_TOOLS=native` gives the model Pi's `read`, `edit`, `write` and `bash` tools again, and
`ULTRON_EXTENSION_TOOLS=native` makes extension and MCP tools separate model tools again.

MCP servers are built in: list them in `~/.ultron/agent/mcp.json` (or `ultron mcp add <name> -- <command>`), manage
them with `/mcp` and `ultron mcp list|login|logout`, and call them from Python. They connect on first use, so they
never slow startup. See [docs/mcp.md](packages/coding-agent/docs/mcp.md).

## Claude Code

Ultron works with the [Claude Code](https://claude.com/claude-code) CLI in three ways:

- as the root agent's model in Ultron's TUI (`ultron --claude`);
- as the host, with Ultron's REPL as its only tool (`ultron claude`);
- as a provider for sub-model frames (`claude-code/*`).

All three run on your Claude Code login (for example a Pro or Max subscription) and check it with
`claude auth status`. Ultron never reads Claude credentials; the CLI logs itself in.

Driving Claude Code headlessly with `claude -p` (stream-JSON in and out, sessions continued with `--resume`, a
single MCP tool as the only door, credentials left to the CLI) is the approach of
[xmpuspus](https://github.com/xmpuspus); Ultron's Claude Code integration follows it.

Claude Opus 5.5 (`claude-opus-5-5`) is the default model of both Claude modes, for the root, the frames and the
sub-agents. Under `ultron --claude`, frames and sub-agents follow the session model unless you set them in
[`/settings → Models`](#models-settings--models).

### Ultron's UI with Claude Code underneath: `ultron --claude`

`ultron --claude` is Ultron as usual (its TUI, runtime, RLM pane, Loki, budgets and session files) with the
root agent's model calls made by Claude Code.

```bash
ultron --claude                           # claude-opus-5-5 through Claude Code
ultron --claude --model claude-code/haiku # another Claude Code model (sonnet, opus, haiku, a full id)
ultron --claude -c                        # continue the last session: its Claude Code session is resumed
ULTRON_ROOT=claude ultron                 # the same as --claude (or the global setting claudeCode.root: true)
```

![ultron --claude: a cell spawns a sub-agent and counts lines while the RLM pane shows both, and the footer shows Claude Code usage](docs/demos/claude-root.gif)

*`ultron --claude --model claude-code/haiku`: one cell spawns a sub-agent (its own Claude Code session) and runs a
shell command, and the RLM pane shows both. The footer shows the subscription windows Claude Code reports.*

How it works: each root run is one headless `claude -p` process speaking stream-json (`--input-format
stream-json --output-format stream-json --include-partial-messages`), started with `--session-id` on the first run and
`--resume` after it; the session id is kept in the Ultron session, so `ultron --claude -c` continues the same Claude
Code conversation. Claude Code gets no tools of its own (`--tools ""`, `--strict-mcp-config`, `--setting-sources ""`,
`--disable-slash-commands`, `--permission-prompts none`) and Ultron's own system prompt (`--system-prompt-file`, with
a note that `rlm` is listed as `mcp__ultron__rlm`). Its one MCP server is a bridge (`ultron mcp --bridge`) back to the
session worker: when Claude calls `rlm`, the call is handed to Ultron's harness as an ordinary tool call, which runs
the cell on the root kernel exactly as in a native turn, and the result goes back to Claude Code.

**The same as native Ultron:** the transcript (streamed text, `rlm` cells with their code and output, errors), the
RLM pane and its waves, Esc to interrupt (the `claude` process group is stopped; the next turn resumes), typing
while a turn runs (the message goes to Claude Code's input queue and reaches the model with the next tool result),
wake-ups (a job, subagent or task that finishes while the root is idle starts a new resumed turn with its
`<runtime_event>`), Loki, per-root turn, token and cost limits (Claude Code's reported
usage and cost are charged per response), `/review`, `/settings`, sessions, `/tree` and forks. Subagents
(`rlm.spawn`) inherit the root's model, so each one is its own Claude Code session on its own Ultron lane and kernel;
frames without tools use the plain `claude-code` provider.

**Different:** Claude Code manages the context (Ultron's compaction is declined for these lanes; the footer shows the
context Claude Code reports), the footer shows `claude-opus-5-5 (Claude Code)` and your subscription windows (`Claude
Code 5h 18% · 7d 63%`), costs are the CLI's notional figures (`(sub)`), and Claude's thinking is not shown (Claude
Code streams none). `/model` can switch to any other model; the transcript carries on there, and switching back to a
Claude Code model starts a new Claude Code session that gets the conversation so far as text.

### Claude Code's UI with Ultron's REPL: `ultron claude`

`ultron claude` runs Claude Code as Ultron's root agent: Claude Code keeps its TUI, its model and your login, and its
only tool is Ultron's REPL.

```bash
ultron claude                             # Claude Code's TUI; the REPL (rlm) is its only tool
ultron claude --watch                     # inside tmux: the RLM view opens in a split beside it
ultron watch                              # the RLM view of the running session, in another terminal
ultron claude -p "why does test_x fail?"  # print mode; any other argument goes to claude
ultron claude --print-config              # the exact claude command and configs, for other tools
ultron guide --for claude                 # the system prompt Claude Code gets
```

![ultron claude --watch in tmux: Claude Code on the left runs one rlm.map, the RLM view on the right shows the cells and the frames](docs/demos/claude-tui.gif)

*`ultron claude --watch --model haiku --frame-model cliproxyapi/glm-5.3-flash` in tmux: Claude Code runs the cells,
and the `rlm.map` frames go to another provider. The RLM view on the right follows the session.*

How it works: `ultron claude` checks `claude auth status` (it never reads Claude Code's credentials or settings
files) and starts `claude` with `--strict-mcp-config --mcp-config` pointing at `ultron mcp`, `--tools ""
--allowedTools mcp__ultron__rlm` (no built-in tools, no permission prompts for the REPL), `--system-prompt-file`
with Ultron's guide in place of Claude Code's default prompt, `--setting-sources ""` (your Claude Code settings,
hooks and CLAUDE.md are not loaded; `--keep-settings` loads them, `--keep-mcp` keeps your MCP servers),
`--dangerously-skip-permissions` (no permission prompts; `ULTRON_CLAUDE_SKIP_PERMISSIONS=off` keeps them, and it is
left out when running as root unless `IS_SANDBOX=1`), and a temporary `--settings` file with three hooks. Its Claude
Code subagents and `ultron --claude` run the same way. The configs live in a private temp dir removed on exit. `ultron mcp` is
an MCP server that runs Ultron's own runtime, so you can also add it to any Claude Code setup yourself
(`claude mcp add ultron -- ultron mcp`, then allow `mcp__ultron__rlm`).

The system prompt (and each Claude Code subagent's) includes your AGENTS.md/CLAUDE.md context files and your skills,
loaded exactly as native `ultron` loads them (same settings and project trust); `--no-context-files` and
`--no-skills` leave them out.

**The same as native Ultron:** the persistent kernel and its skills, output truncation, hints (stuck loops,
polling, detached jobs), secret masking, Loki guardrails, `rlm.load`/`infer`/`map` frames, subagents with checked
verdicts, typed agents, workflows, shell jobs, the usage ledger, kernel snapshots, and memory. Each Claude Code
session has its own Ultron session, found again by Claude Code's session id, so `claude --resume` reopens the same
tasks, `state` and ledger.

**Different:**

- Claude Code owns the conversation: its context, compaction and model loop. `ctx` context edits and Ultron's own
  TUI panels are not available; `ultron watch` shows the RLM view (graph, waves of subagents, frames, jobs,
  verdicts, kernels and Loki stats) in a second terminal, or in a tmux split with `--watch`.
- Nothing can wake Claude Code between turns. A job, subagent or task that ends while it is not waiting is reported
  at the top of the next `rlm` result, or with your next message (UserPromptSubmit hook); the guide says so.
- Hooks: SessionStart adds Loki's note; UserPromptSubmit delivers waiting events; Stop closes the turn's budget.
- Frames run on `claude-code/claude-opus-5-5` (single completions through `claude -p`) when that provider is
  available, else on your default Ultron model; `--frame-model` changes it. Subagents are Claude Code processes by
  default (`claude -p` with their own `ultron mcp --child` server and kernel, reporting their verdict to the parent's
  host) on `claude-opus-5-5`, which `--child-model` changes. `--children ultron` runs them as Ultron lanes on your
  default model instead.
- Per-turn budgets count frames and subagents; the root's own usage is Claude Code's.

**Fair use.** Everything runs on your Claude Code login. Each frame and each subagent is its own Claude Code request
or process and counts against your plan's limits. By default all of them run Opus 5.5, so move the high-volume work
when a task fans out: `--frame-model claude-code/haiku` (or any `provider/model`) for frames, and
`--child-model sonnet` for subagents. The other defaults are conservative: subagents may not delegate further unless
given `depth=`, a turn admits at most 24 tasks, and the guide steers toward searching with code before any frame.

### Claude Code as a frame model

With the CLI installed and logged in, the `claude-code` provider runs model calls through it (`claude -p`) with no
API key, in any mode: `claude-code/haiku`, `claude-code/sonnet`, `claude-code/opus`, `claude-code/claude-opus-5-5`.
Tool-free calls serve inference frames, `/review` frames and judges. A lane with tools (the root agent under
`ultron --claude`, its subagents) runs as a Claude Code session whose tools are served back to Ultron over MCP.

```bash
ULTRON_RLM_FRAME_MODEL=claude-code/haiku ultron   # code-free rlm.infer/rlm.map frames go to the CLI
```

Or pick it in [`/settings → Models`](#models-settings--models) without restarting. Each call is isolated (no tools,
MCP servers, settings, hooks or `CLAUDE.md`, and its own system prompt) and uses your subscription's shared limits, so
at most four run at once (`ULTRON_CLAUDE_CODE_CONCURRENCY`). Details:
[providers](packages/coding-agent/docs/providers.md#claude-code-cli-claude-code).

## Optional services

- **Hindsight** memory is used at `http://localhost:8888` when it is running. `ultron setup` can install it with
  Docker or save another address as the `hindsightUrl` setting; `ULTRON_HINDSIGHT_URL` overrides both, and `off`
  disables memory. Without it, memory calls fail quietly and turns are unaffected.

## Loki guardrails

[Loki](https://github.com/alfredosdpiii/loki) checks every file Ultron writes with deterministic rules and real
analyzers (hardcoded secrets, XSS and injection sinks, protected files, net-new Ruff/mypy/tsc/Oxlint/Clippy/Credo
findings). It is bundled and on by default:

- `edit()` and `write()` are checked before the file changes; a finding raises `ValueError` and nothing is written.
  A check that takes longer than 5 s (`ULTRON_LOKI_TIMEOUT_MS`) lets the write through with a visible "Loki did not
  check this write" note.
- A `bash()` command that would write a source or configuration file in the project (a redirect, a here-document,
  `tee`, `sed -i`, `perl -i`, `cp`/`mv`/`install`, `dd of=`) is refused before it runs, with "bash command was not
  run" and Loki's reason, so a shell write cannot skip the check above; use `edit()`/`write()` instead. Writes into
  notes and data (`.md`, `.txt`, `.log`, `.patch`, ...), Git-ignored output, `/dev/null` and paths outside the project
  run as before, and commands that write no file never reach Loki.
- Files a cell changes any other way (`Path.write_text`, a program or build step) are checked after the cell, in the
  background; findings arrive with the next cell result. Only what the cell introduced is reported, in three kinds
  that are never mixed: new findings ("fix these"), advisory lines such as a function grown complex (an FYI, no
  change required), and checks that could not run (a missing analyzer, said once per session). Existing code in an
  edited file is not reported.
- A newly imported Python module is checked against the project's interpreter, not the one Loki runs on:
  `ULTRON_LOKI_PROJECT_PYTHON`, else the `python` that `bash()` runs when it belongs to a virtual or conda
  environment, else a `.venv`. Without one the import is "not checked", never "unresolved".
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

![Loki blocks an edit that would write a live-looking Stripe key into config.py](docs/demos/loki.gif)

*Asked to put a made-up live Stripe key into `config.py`, the model's `edit()` raises `loki/secret: hardcoded
credential (Stripe live key)` and the file stays unchanged. glm-5.3-flash.*

`npm run loki:update -- <loki-checkout | version>` refreshes the bundled engine, pinned by sha256.

## Safety

There is no sandbox. Model-written Python runs with your user's permissions, like Pi's `bash` tool. Resource limits
apply (the process-tree memory cap, CPU and wall-time budgets, per-turn token and turn limits), and Loki and secret
masking catch some mistakes, but none of this is isolation. Run Ultron in a container or VM if you need a boundary;
Pi's [containerization guide](packages/coding-agent/docs/containerization.md) applies.

## Where the ideas come from

Ultron combines ideas from several projects and papers:

- **Prime Intellect's RLM harness**: the REPL as the sole tool, shell and MCP as Python skills, output truncation.
- **NVIDIA's NOOA**: bounded previews instead of dumps, and agents as typed Python classes.
- **Autolith**: bounded inference over inputs the root never reads, with contracts, repair and budget trees.
- **LLM-as-Code**: context that collapses when work returns, and self-improvement committed as tested code.
- **Unreal Agent**: long work that never blocks the turn.
- **waku-agent**: memory behind a retrieval gate.
- **[xmpuspus](https://github.com/xmpuspus)**: driving Claude Code headlessly through `claude -p`, the basis of
  `ultron claude`, `ultron --claude` and the `claude-code/*` provider.
- **[pstack](https://github.com/cursor/plugins/tree/main/pstack)** by Lauren Tan ([poteto](https://x.com/poteto)):
  the engineering skills bundled with Ultron (`/skill:rigor` and the skills it runs), ported to the REPL. See
  [packages/coding-agent/skills/pstack](packages/coding-agent/skills/pstack/README.md).
- **[HumanLayer](https://github.com/humanlayer/skills)**: the `show-me` skill, bundled as `/skill:diagram-it` with
  terminal-first views drawn from the code. See
  [packages/coding-agent/skills/humanlayer](packages/coding-agent/skills/humanlayer/README.md).
- **[Answer me with HTML](https://github.com/QingYunA/answer-me-with-html)** by QingYunA: bundled as
  `/skill:answer-me-with-html`, which answers a hard question with one HTML page (panels, diagrams, tables) rendered
  by its own `am` CLI. See [packages/coding-agent/skills/qingyuna](packages/coding-agent/skills/qingyuna/README.md).
- **[no-mistakes](https://github.com/kunchenguid/no-mistakes)** by Kun Chen: bundled as `/skill:no-mistakes`, which
  drives the `no-mistakes` CLI (installed separately) to gate committed work through review, tests, lint, push, PR
  and CI. See [packages/coding-agent/skills/kunchenguid](packages/coding-agent/skills/kunchenguid/README.md).

[`supremeplan.md`](supremeplan.md) records how each one landed and what was measured.

## Development

```bash
npm install --ignore-scripts
npm run build:offline           # build all packages without refreshing model data
npm run check                   # lint, format, type check
./test.sh                       # all tests
npm run test:acceptance         # acceptance rows A01-A56, judged by the runner
npm run scan:secrets            # credentials and home-directory paths in tracked files
node scripts/pack-release.mjs   # build the self-contained release tarball (after a build)
npm run publish:npm -- --dry-run   # build, pack and check the ultron-agent npm package (drop --dry-run to publish)
docs/demos/record.sh            # re-record this README's demo GIFs (makes real model calls; see the script)
```

Design and status: [`docs/implementation-status.md`](docs/implementation-status.md),
[`docs/ultron-architecture.md`](docs/ultron-architecture.md), [`supremeplan.md`](supremeplan.md).

## Credits and license

Ultron is built on [Pi](https://github.com/badlogic/pi-mono) by Mario Zechner and contributors, and keeps Pi's
MIT license (see [LICENSE](LICENSE)). It bundles [Loki](https://github.com/alfredosdpiii/loki) (MIT, its license in
`packages/coding-agent/src/ultron/loki-engine/LICENSE`) and skills ported from
[pstack](https://github.com/cursor/plugins/tree/main/pstack) by Lauren Tan (MIT, its license in
`packages/coding-agent/skills/pstack/LICENSE`) and HumanLayer's [`show-me`](https://github.com/humanlayer/skills) skill
as `diagram-it` (MIT, its license in `packages/coding-agent/skills/humanlayer/LICENSE`), and QingYunA's
[Answer me with HTML](https://github.com/QingYunA/answer-me-with-html) skill with its `am` CLI (MIT, its license in
`packages/coding-agent/skills/qingyuna/LICENSE`), and Kun Chen's
[no-mistakes](https://github.com/kunchenguid/no-mistakes) skill (MIT, its license in
`packages/coding-agent/skills/kunchenguid/LICENSE`). Pi's documentation at [pi.dev](https://pi.dev) covers the interface,
providers, extensions and settings that Ultron shares.
