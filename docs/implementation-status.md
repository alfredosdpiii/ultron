# Ultron implementation status

Ultron is a fork of Pi whose only agent runtime is the native RLM session worker. This file describes what is built. Whether each A01-A46 row is proven is decided by the acceptance runner, not by this file: see [a01-a46-acceptance.md](a01-a46-acceptance.md).

## Runtime

- Every agent run goes through the native session worker: the TUI, `-p`/`--mode json`, and `--mode rpc`. `--help`, `--list-models`, and `--export` never run the agent and use Pi's handlers. `--export` renders native session files.
- `--mode rpc` speaks Pi's JSONL protocol (`src/experimental/rpc-native.ts`); Pi's `RpcClient` drives it in tests. Differences from Pi: model objects carry `provider`, `id`, `name`, `reasoning`; `get_tree` returns the active branch; entries use the native format; extensions run headless, so there are no `extension_ui_request` events. The Ultron-specific `inspect` command exposes the read-only inspector.
- Extension slash commands are registered directly (`/name`, as in Pi) and via `/extension <name>`. `/agents`, `/memory`, `/skills`, `/experiments`, `/goals`, and `/progress` read recorded runtime state without starting work or searching.
- Busy session workers survive the client exiting: background jobs and child lanes keep running, the next `ultron` reattaches, and idle workers retire.
- `ultron migrate import-pi | export-pi | backup | restore` moves Pi sessions in and out and rehearses profile backup and restore.

## RLM host

- Each lane (the root and every task) has its own persistent Python kernel. Kernels receive no credential-like environment variables and no worker control channel (`ULTRON_RLM_ENV_ALLOW` passes named variables through). A pool caps live kernels (16), evicts idle ones after a snapshot, and never evicts a running cell. Snapshots use a checksummed, constrained JSON format.
- The host knows which lane issued each request, so tasks record their parent, child lanes can only query their own subtree, and cancelling a task stops its whole subtree.
- Strategies: `deterministic`, `predict` (one tool-free model call with bounded repair), and `rlm` (a model lane). All share admission, idempotency, timeouts, usage accounting, and the durable journal. Results are always `verification: "unverified"`; only explicit checks (progress reassessment, goals, release gates) can say more.
- `agents.spawn` and `rlm.spawn` tasks outlive the Python cell that started them; `agents.invoke` waits inside the cell. A child's deadline is capped at the root's remaining wall budget.
- Workflows validate before any effect and support fan-in, conditional routes, explicit skips and failures, and keyed nodes.
- Host modules (`src/ultron/*.ts`, Python classes in `src/ultron/rlm/*_api.py`):
  - `schedules` / `goals`: slot-keyed schedules that never double-fire, missed ticks coalesce, goals achieved only by passing required checks.
  - `skills`: version-pinned (content hash), explainable selection; frontmatter cannot grant capabilities.
  - `agent_message`: parent/child messaging with verified senders, deduplication, expiry, inbox caps, optional steering, and verifier isolation.
  - `progress`: evidence receipts, progressing/busy/stalled/finished assessment, completion claims verified only by a passing verifier; budgets never extend.
  - `instances`: retain a completed task and continue it on its own lane with fresh scratch and a persistent `state` dict.
  - `grants`: scope/revision/policy/owner/expiry-bound approvals, rechecked on use; dormant by default.
  - `gates`: frozen release-gate definitions and baseline/candidate comparison where required regressions block.
- Refinements (versioned lessons) reach later task prompts, follow the conversation branch (a lesson from an abandoned branch never applies), refuse protected targets and capability requests, and are content-validated. Approval is off by default and recorded as `not_required`.
- Memory (Hindsight via Jev gating) withholds forgotten and superseded items even when the backend still returns them, labels evidence classes, records scope denials, and reuses recorded decisions.

## Controls

All optional controls (permission prompts, risk blocking, capability enforcement, budget enforcement beyond admission, completion gates, refinement approval, mandatory sandbox) are off by default, reported as off by `agents.status`, and never prompt.

## Known limits

- Resource limits are the trusted-local profile: output, artifacts, wall time, admission, and process-group kills. There are no memory or CPU limits and no isolated sandbox profile.
- Without a sandbox, a model can read anything the user can, including test files outside the project. The live A46 run observed the model reading its own demonstration test; hostile-code or blind evaluation needs real isolation.
- Snapshot checksums detect corruption, not a writer who recomputes the digest.
- Quitting in the middle of a root turn still stops that turn; work started by earlier turns continues. A hard-killed client leaves idle work to a 30-second orphan grace.
- Workflows have no any-of joins or bounded revision cycles.
- Pi import brings in the active branch only; labels and model changes are reported as skipped.
