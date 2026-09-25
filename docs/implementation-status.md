# Ultron implementation status

Ultron is a fork of Pi whose only agent runtime is the native RLM session worker. This file describes what is built. Whether each A01-A46 row is proven is decided by the acceptance runner, not by this file: see [a01-a46-acceptance.md](a01-a46-acceptance.md).

## Runtime

- Every agent run goes through the native session worker: the TUI, `-p`/`--mode json`, and `--mode rpc`. `--help`, `--list-models`, and `--export` never run the agent and use Pi's handlers. `--export` renders native session files.
- `--mode rpc` speaks Pi's JSONL protocol (`src/experimental/rpc-native.ts`); Pi's `RpcClient` drives it in tests. Differences from Pi: model objects carry `provider`, `id`, `name`, `reasoning`; `get_tree` returns the active branch; entries use the native format; extensions run headless, so there are no `extension_ui_request` events. The Ultron-specific `inspect` command exposes the read-only inspector.
- Pi extensions from the profile run inside the session worker through the legacy adapter. To stop one loading without deleting it, add an exclusion to the `extensions` setting, for example `"extensions": ["-extensions/jev/index.ts"]` in `<agentDir>/settings.json`. The setting takes exact `-path`, glob `!pattern`, and `+path` forms, with paths relative to the agent directory. This is how to retire the Pi Jev extension. Ultron now does its automatic memory natively. Its model pin and tool guard are deliberately not ported.
- Extension slash commands are registered directly (`/name`, as in Pi) and via `/extension <name>`. `/agents`, `/memory`, `/skills`, `/experiments`, `/goals`, and `/progress` read recorded runtime state without starting work or searching.
- Busy session workers survive the client exiting: background jobs, child lanes, and a root turn in flight keep running, the next `ultron` reattaches, and idle workers retire. Leaving the app (Ctrl-C/Ctrl-D in the TUI, a signal, closing RPC stdin) is not an abort; Esc in the TUI and RPC `abort` are. After a hard kill, idle workers exit within about a second and busy ones finish first.
- `ultron migrate import-pi | export-pi | backup | restore` moves whole Pi session trees in and out (every branch, labels, model and thinking-level history, context edits; Pi-only entry types kept as `pi-session:<type>` custom entries) and rehearses profile backup and restore.
- `/rlm` (Ctrl+R) shows a live RLM panel: the task tree with states and results, the root kernel's current cell, admitted tasks, wall time and cost, and the kernel pool. `/jev` (Alt+J) shows Jev's recent decisions (triage routes, recall gates, retention) from a bounded, hashed decision log, and whether Jev and Hindsight are available. Both collapse to a footer line.

## RLM host

- Each lane (the root and every task) has its own persistent Python kernel. Kernels receive no credential-like environment variables and no worker control channel (`ULTRON_RLM_ENV_ALLOW` passes named variables through). A pool caps live kernels (16), evicts idle ones after a snapshot, never evicts a running cell, and pins retained instances' lanes (up to half the pool). Kernels run under per-process memory limits (`ULTRON_RLM_MAX_MEMORY_MB`, default 4096) and a per-cell CPU budget (`ULTRON_RLM_MAX_CPU_SECONDS`, default 1800); exceeding either fails the cell clearly and the kernel is replaced if needed. Snapshots are HMAC-signed by the host with a per-profile key (`<agentDir>/rlm-snapshot.key`); unsigned or tampered snapshots are refused.
- The host knows which lane issued each request, so tasks record their parent, child lanes can only query their own subtree, and cancelling a task stops its whole subtree.
- Strategies: `deterministic`, `predict` (one tool-free model call with bounded repair), and `rlm` (a model lane). All share admission, idempotency, timeouts, usage accounting, and the durable journal. Results are always `verification: "unverified"`; only explicit checks (progress reassessment, goals, release gates) can say more.
- `agents.spawn`, `rlm.spawn`, and module-started tasks outlive the Python cell that started them; `agents.invoke` waits inside the cell. Each root turn gets its own wall budget (`ULTRON_MAX_WALL_MS`, default 30 minutes) and admission cap (`ULTRON_MAX_ADMITTED_TASKS`, default 24); a child's deadline is capped at its root's remaining budget. An optional cost cap (`ULTRON_MAX_COST_USD`, unset by default) refuses new model-backed work once reached or when pricing is unknown.
- Workflows validate before any effect and support fan-in, conditional routes, any-of joins (`join: "any"`), bounded revision loops (`revise: {from, until, max_rounds <= 10}`, each round its own task), explicit skips and failures, and keyed nodes and workflows.
- Host modules (`src/ultron/*.ts`, Python classes in `src/ultron/rlm/*_api.py`):
  - `schedules` / `goals`: slot-keyed schedules that never double-fire, missed ticks coalesce, goals achieved only by passing required checks.
  - `skills`: version-pinned (content hash), explainable selection; frontmatter cannot grant capabilities.
  - `agent_message`: parent/child messaging with verified senders, deduplication, expiry, inbox caps, optional steering, and verifier isolation.
  - `progress`: evidence receipts, progressing/busy/stalled/finished assessment, completion claims verified only by a passing verifier; budgets never extend.
  - `instances`: retain a completed task and continue it on its own lane with fresh scratch and a persistent `state` dict.
  - `grants`: scope/revision/policy/owner/expiry-bound approvals, rechecked on use; dormant by default.
  - `gates`: frozen release-gate definitions and baseline/candidate comparison where required regressions block.
- Refinements (versioned lessons) reach later task prompts, follow the conversation branch (a lesson from an abandoned branch never applies), refuse protected targets and capability requests, and are content-validated. Approval is off by default and recorded as `not_required`.
- Memory uses Hindsight at `http://localhost:8888` by default (bank `ultron`, created on first use; `ULTRON_HINDSIGHT_URL` overrides, `off` disables). Automatic memory is gated by Jev; deliberate `memory.prepare`/`memory.propose` calls from agent code skip the relevance gate, and writes Jev rates sensitive are still refused. Memory withholds forgotten and superseded items even when the backend still returns them, labels evidence classes, records scope denials, and reuses recorded decisions.
- Automatic per-turn memory (`src/ultron/auto-memory.ts`) runs on root-lane runs only, never on child task lanes, and needs both Hindsight and Jev. Before a run, the user's request goes through Jev's recall gate (the non-explicit path: a low score makes no Hindsight call). Recalled evidence is injected as an untrusted `ultron-memory` custom message. That message stays in the transcript, the TUI shows it muted, and `memory.why("auto:<runId>")` explains it. After a completed run, Jev's retention policy judges the request and the answer. Only a keep with confidence of at least 0.65 is stored, as `[User] … [Assistant] …`. Aborted or failed runs are never stored. Scope is `project` by default (`ULTRON_AUTO_MEMORY_SCOPE=session|project|global`). `ULTRON_AUTO_MEMORY=on|recall|off` (default `on`) controls it. Recall and retain decisions appear in `jev.decisions` and the `/jev` panel. A Jev or Hindsight failure is swallowed, so the turn runs without memory. Jev reads `TYPESAFE_API_KEY` or, like the Pi extension, `<agentDir>/jev-api-key`. When the gate retrieves, the Pi Jev extension's Hindsight bank (`ULTRON_HINDSIGHT_LEGACY_BANK`, default `omp`; `off` disables) is also searched read-only, without tags. Up to 12 deduplicated results are appended under "Earlier memory (from Pi, read-only)". A missing bank counts as empty, and Ultron never writes to it.
- Ultron does not route or pin models from Jev triage. The model is the one from `--model`, `/model`, or settings. Triage is available to agent code as `jev.triage`.

- A research-loop brake steers the root agent to answer after 10 consecutive tool-call rounds in one turn, and again more firmly at 20 (`ULTRON_TOOL_ROUNDS_NUDGE`, 0 disables). It never aborts; Esc does.

## Controls

All optional controls (permission prompts, risk blocking, capability enforcement, completion gates, refinement approval, mandatory sandbox, cost cap) are off by default, reported by `agents.status`, and never prompt.

## Known limits

- There is no sandbox. Memory limits are per process (no cgroup total across a kernel's process tree).
- Without a sandbox, a model can read anything the user can, including test files outside the project. The live A46 run observed the model reading its own demonstration test; hostile-code or blind evaluation needs real isolation.
- The snapshot key lives on disk as the same user, so model code could read it; signing stops tampering by anyone without the key.
- Pi import: a context edit that only some branches carry is kept as an entry but not applied; Pi `usage` entries are kept but not added to native usage totals.
