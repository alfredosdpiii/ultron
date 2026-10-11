# crates/harness: durable task runtime port plan

Scope: the RLM task runtime (`packages/coding-agent/src/ultron/rlm/task-store.ts` and the task supervision half of
`native-host.ts`) and Ultron's agent-core harness (`packages/agent/src/harness`), onto `crates/durable` (storage,
sessions, documents) and `crates/agent` (agent loop). RLM-first: the model's only tool is the REPL; everything here is
host machinery behind kernel APIs (`rlm.spawn/collect`, `agents.*`, workflows). No native model tools, no
host-driven chat loop.

## Ultron-specific vs upstream Pi Durable

| Source | Origin | Notes |
| --- | --- | --- |
| `ultron-rust-reference/packages/durable/src/harness/*` (scheduler 1.5k lines, task-graph, submissions, inbox, generation, tool, registry, view) | upstream Pi Durable | Kind/phase task kernel over Durable storage. Scheduler, task runtime, task registry, define and util ported to `src/scheduler/`; submissions, inbox, live, generation, tool (as the REPL cell), compaction, context, prompt and usage ported to `src/chat/` (see below); task-graph, view, events, agents/extension registry not yet. |
| `packages/agent/src/harness/pico3/*` | Ultron fork, experimental (`./experimental/pico3`; used only by `coding-agent/src/experimental/micro`) | An earlier, smaller sibling of the upstream Durable harness (tasks with `after` deps, phases, checkpoints, in-flight phases, abort marking, `faulted`). If the product adopts a phase kernel, port upstream Durable's harness instead; do not port pico3. |
| `packages/agent/src/harness/session/*` (Session, values, commit, jsonl/) | Ultron fork of Pi agent-core | Ultron's live session format (lanes, operations, inbox, values such as `ultron.tasks/root`); not the Durable schema. |
| `packages/agent/src/harness/runtime/*` (lane, drive/*, reducer, restore, progress, transcript) | Ultron | Production run engine: one `Lane` per REPL lane; operations admitted, driven, checkpointed, recovered. |
| `packages/agent/src/harness/execution/*`, `tools/*` | Ultron fork of Pi | Effect gate and tool execution; bash/read/write/edit are `crates/host` operations behind the REPL. |
| `coding-agent/src/ultron/rlm/task-store.ts`, `native-host.ts` (task part) | Ultron | RLM task journal and supervisor. Ported here. |

## File-by-file mapping

| TypeScript | Rust | Status / evidence |
| --- | --- | --- |
| `rlm/task-store.ts` (`NativeTaskJournal`, `taskFingerprint`, bounded persistence) | `src/task_store.rs` | Ported. `fixtures/generate.mjs` -> `task-store.json`; `tests/typescript_fixture.rs`. |
| `rlm/native-host.ts`: `request`/`taskOptions`/`definitionKey`, `spawnTask` admission, `run` (running transition), `finish` (first terminal wins), `cancel` (subtree, `Ancestor X cancelled: ...`, abort once), `close`, `handle` for `agents.spawn/inspect/result/cancel/status` with child-lane visibility, `loadTasks` recovery | `src/supervisor.rs` (`TaskSupervisor`, `SpawnRequest`, `CancelSignal`) | Ported. `fixtures/generate-supervisor.mjs` drives the real `NativeRlmHost` with controlled lanes -> `task-supervisor.json`; `tests/supervisor_fixture.rs`; unit tests for close-before-use and failed writes. |
| `rlm/native-host.ts`: timers (`timeout_ms`, root wall deadline), usage reservations, worktrees, verdict check (`childResult`), lane execution, workflows, modules, graph view | `crates/harness` (timers, executor glue), `crates/inference`/`crates/host` for the rest | Remaining. Timeouts need the async runtime glue (a timer per task calling `cancel`). |
| `createSessionTaskStore` (session value `ultron.tasks/root`) | `TaskDocumentStore` impl over the Ultron session value store | Remaining: needs session values. |
| `harness/runtime/reducer.ts` | `src/reducer.rs` (over the JSON `LaneSnapshot`/`HarnessEvent` shapes) | Ported. `fixtures/generate-reducer.mjs` records real faux-provider lane runs (resnapshot) and synthetic sequences -> `reducer.json`; `tests/reducer_fixture.rs`. |
| `harness/execution/effect-gate.ts` | `src/effect_gate.rs` | Ported; createGate cases of `test/harness/execution-primitives.test.ts` plus transition tests. |
| `harness/session/mutation-line.ts` | none | Not needed: `&mut self` / a `Mutex` serializes mutations. |
| `harness/session/{types,values,commit,session,memory,in-memory-storage-state}.ts` | `src/session/` over `durable::Storage` atomic commits | Remaining. Values first (`getValue/setValue/scanValues`): the task journal, lane state and operation state live there. |
| `harness/session/jsonl/*` (+ `legacy-v3.ts`) | `src/session/jsonl.rs` (reuse `durable::JsonlStorage` framing where formats match) | Remaining; fixture-test against copied TS session dirs, never live ones. |
| `harness/runtime/types.ts` (`LaneState`, `Drive`) | `src/runtime/` | Remaining; `Drive` wraps `effect_gate` + a completion channel. |
| `harness/runtime/lane.ts` (accept, drive, requestOperationAbort, resume, abort, steer/nextRun/followUp/cancelQueued, waitForIdle, watch) | `src/runtime/lane.rs` | Remaining. Needs session values. Abort: `begin_abort` on the drive's gate, commit `cancel_requested` + drained steer/followUp, then `signal_abort` (or on the repeat path). |
| `harness/runtime/drive.ts`, `drive/{boundary,checkpoint,deferred,reconcile,recovery,retry,terminal}.ts` | `src/runtime/drive/` | Remaining. Recovery: an unfinished operation on reopen is reconciled from its checkpoint; unsafe in-flight tools get the interrupted result, never re-run. |
| `drive/{generation,response,tools,tool-placement,structural}.ts` | `src/runtime/drive/` calling `crates/agent` / `crates/ai` | Remaining. The single tool is the REPL via `crates/kernel`. |
| `harness/runtime/{restore,progress,transcript}.ts` | `src/runtime/` | Remaining. |
| `harness/execution/{tools,assistant}.ts`, `hooks.ts`, `events.ts` (event bus) | `src/runtime/execution.rs`, `src/events.rs` | Remaining. |
| `harness/tools/*` | `crates/host` | Owned there. |
| `harness/compaction/*` | `crates/compaction` | Owned there. |
| `harness/pico3/*` | none | Not ported (see above). |

## Upstream Pi Durable scheduler (`src/scheduler/`)

| Upstream | Rust | Evidence |
| --- | --- | --- |
| `harness/scheduler.ts` (live mirror, open reconcile, resume, reservation, step rules, handover, definition blocking, ownership walks, cascades, failFast, completing holds, abandonOnRestart, orphaning, join) | `scheduler/mod.rs` (`Scheduler`) | `tests/scheduler_{phases,scheduling,recovery,ownership,failure}.rs` |
| `TaskScheduler.#runtime` | `scheduler/runtime.rs` (`TaskRuntime`) | same |
| `harness/util.ts` (`Waiters`, `closedError`), Context/AbortSignal | `scheduler/cancel.rs` (`Cancel`, `Ctx`, `Slot`, `Waiters`) | same |
| `harness/define.ts` `defineTask`, task part of `registry.ts` | `scheduler/registry.rs` | same |
| task half of `harness/harness.ts` (open, close, closed, safeNow, safeReport) | `scheduler/harness.rs` (`Harness`, `Conversation`, `Harness::reopen`) | same; kill/reopen in `tests/scheduler_kill.rs` |

Threads, not async: the durable Session is synchronous with a FIFO line; one worker thread per Harness runs reconcile and
reservation, one thread per invocation runs handlers. Durable access the scheduler needs beyond the public API lives in
`crates/durable/src/{transaction,session}/scheduler_access.rs`.

## Conversation runtime (`src/chat/`, upstream `harness/{harness,submissions,inbox,live,generation,tool,compaction,context,prompt,usage}.ts`)

RLM-first: the model's tool list is exactly the REPL tool a [`CellExecutor`] declares; the tool task (`pi.tool`) is the
durable REPL cell. `ChatHarness` wraps the task `Harness`, installs `pi.generation`, `pi.tool`, `pi.compaction` and
`ultron.child` into the application's registry, creates every conversation with `pi.live`/`pi.inbox`/`pi.usage`/
`pi.provider`/`pi.agent`, and withdraws queued inputs from `pi.inbox` (the scheduler's raw-submission emulation is gone).

| Upstream | Rust | Evidence |
| --- | --- | --- |
| `submissions.ts`, `inbox.ts` | `chat/{submissions,inbox}.rs` | `tests/chat_submissions.rs`, `tests/chat_inbox.rs` (ported, Memory/SQLite/JSONL); `tests/scheduler_ownership.rs` now queues through the inbox |
| `live.ts`, `usage.ts`, `provider.ts`, agent doc | `chat/{live,usage,docs}.rs` | `tests/chat_inbox.rs` usage cases |
| `generation.ts`, `prompt.ts`, `context.ts` | `chat/{generation,prompt,context}.rs` | `tests/chat_generation.rs` (generation + generation-recovery) |
| `tool.ts` (model-issued calls) | `chat/cell.rs` (REPL cell, intent = code + lane + replay policy), `chat/kernel_cells.rs` (`crates/kernel` lanes) | `tests/chat_cells.rs` (tools-recovery), `tests/chat_cell_kill.rs` (SIGKILL during a real kernel cell, reopen) |
| `compaction.ts` | `chat/compaction.rs` (summary request from `crates/compaction`) | `tests/chat_compaction.rs` |
| pi-ai `faux`, `utils/{overflow,retry}.ts` | `chat/{faux,models}.rs`; `StreamFnModels` adapts an `agent::StreamFn` | the tests above |
| `task-store.ts` journals (both Rust ports) | `chat/children.rs` (`ultron.child` tasks), `chat/migrate.rs` (import), `delegation::task_store` now an adapter over `task_store.rs` | `tests/chat_children.rs`, `tests/chat_migrate.rs` (TS-recorded documents) |
| Swarm items (rewriteplan) | `chat/{swarm,host_ops}.rs`, `packages/coding-agent/src/ultron/rlm/swarm_api.py` | `tests/chat_swarm.rs`, `tests/chat_swarm_python.rs` (runtime.py, SIGKILL/reopen) |

Composition-root adoption (crates/cli, crates/daemon): build `KernelCells` with the stack's lanes and a handler
factory `|call| HostOps::handler(call, Some(stack_handler))`, `StreamFnModels` over the provider stream, and
`ChatHarness::open(storage, options)`; submit user input with `Conversation::submit`, render from `pi.live`/entries.

Not ported yet: nested tool calls (`executeTool`), tool hooks (`beforeTool`/`afterTool`/`afterTools`), section
registry/wrappers (sections come from one `SectionSource`), background threshold compaction cases beyond the
threshold check, events/view/task-graph, and `agents.*` typed agents as durable tasks.

## Next slices

1. Ultron session values over `crates/durable` storage; `TaskDocumentStore` on `ultron.tasks/root` (journal survives a
   real reopen).
2. Task timeouts and the async executor glue: one task future per spawn, `CancelSignal` -> lane abort, timer ->
   `cancel(reason)` (fixture: the TS host with fake timers).
3. Lane admission and queues, then drive + checkpoint + recovery, then `requestOperationAbort` over the effect gate
   (fixtures recorded from `Lane` with the faux provider, as `generate-reducer.mjs` already does).
4. Kill/reopen test: operation committed, tool intent committed, process killed, reopened: replay-safe reruns, unsafe
   ones yield interrupted.

## Fixtures

All generators are deterministic (run twice, same sha256); run from the worktree root with private
`ULTRON_CODING_AGENT_DIR`/`ULTRON_SERVER_DIR` temp dirs:

- `node crates/harness/fixtures/generate.mjs` -> `task-store.json`
- `node crates/harness/fixtures/generate-supervisor.mjs` -> `task-supervisor.json`
- `node crates/harness/fixtures/generate-reducer.mjs` -> `reducer.json` (resolves `@ultron/*` to package sources;
  pins `Date.now`, `Math.random`, `randomUUID`; renames UUIDv7 IDs)
