# Ultron Rust rewrite

## Status

Implementation is in progress. The first unit, the Chord delta op layer (`crates/chord-delta`), passes its fixture tests.
The second unit, Durable storage (`crates/durable`), replays every upstream storage conformance call on the memory, SQLite and JSONL backends; Rust SQLite files and JSONL directories match upstream's on disk.
The parity gate (`npm run rust:parity`) is red by design: 169 of 241 ledger rows have passing Rust evidence.
The requested scope is the full Ultron product plus upstream Pi Durable, not a Rust launcher around Node.js.

- Branch: `rust-rewrite`, based on Ultron `16c41aa882f0d6c6fb7622d71102b85957021436` (0.87.39).
- Worktree: `../ultron-rust`, relative to the existing Ultron checkout.
- Pi reference checkout: `../ultron-rust-reference`.
- Upstream: <https://github.com/earendil-works/pi>, revision `42a3497d03ad17e308a2299fa824727894f2c0ec`.
- Package: `@earendil-works/pi-durable` 1.1.0, `packages/durable`.
- Reference checkout includes `packages/durable`, `packages/chord`, and `packages/ai`, with the upstream MIT license.

The existing checkout and live Ultron installation remain unchanged. No commits, pushes, or releases are authorized by this plan.

## Scope and completion criteria

[baseline.json](docs/rust-rewrite/baseline.json) records the pinned inputs, package census, and all 56 existing acceptance rows.
The census contains 922 tracked source files and 266,078 source lines across 13 packages, including generated files and comments.
It also identifies 765 test files. These counts describe the reference surface; they are not a time estimate or a requirement to translate files one-for-one.
Upstream Durable has 73 TypeScript source files and 57 `*.test.ts` files.

Completion requires all of the following:

1. Rust implements every user-visible behavior inventoried from the pinned source, or the user explicitly approves a removal.
2. Each acceptance row A01–A56 has Rust execution evidence. The existing TypeScript acceptance report cannot certify the Rust binary.
3. Pi Durable storage, session, harness, task, tool, document, event, and recovery behavior has passing Rust conformance tests derived from upstream behavior.
4. Provider coverage includes authentication, streaming, tool calls, cancellation, usage, retries, and error handling, not only an OpenAI-compatible chat endpoint.
5. The CLI, interactive terminal, JSON/RPC modes, server/client protocol, MCP, settings, skills, context, delegation, workflows, and safety checks operate through the Rust implementation.
6. A release build works in an isolated installation with no Node.js dependency in the core execution path. Any remaining language runtime is an explicit product contract, not a hidden fallback.
7. Rust formatting, Clippy with warnings denied, unit tests, integration tests, crash/restart tests, and product acceptance tests pass. Retained TypeScript code also passes the repository checks.
8. Real terminal and subprocess checks run in private agent/server directories. Existing live sessions and credentials are never used as test fixtures.

A single automated completion command does not exist yet. Phase 1 must create the Rust verification runner; until then, this plan is not executable proof of completion.
Paid provider tests and publishing require separate authorization.

## Settled architecture: Rust host, Python REPL

The user approved a Rust rewrite with Python retained as the model-facing execution language.
Rust owns the agent loop, providers, durable engine, session lifecycle, scheduling, process supervision, MCP integration, server/client protocol, and terminal.
Python remains the computational workspace for cells, agents, workflows, and executable skills. This is not a Python-free product.
The production host must not delegate its implementation to the old TypeScript application.

```text
Rust CLI / terminal / server
              |
Rust agent runtime + provider transports
              |
Rust Pi Durable: storage, sessions, tasks, documents
              |
Rust process supervisor <--> Python kernel subprocesses
                            persistent namespace + async execution
                            bounded results + host callbacks
```

Host operations reached from Python go through a validated protocol. Durable task identity, cancellation, budgets, and completion state belong to Rust,
not to a serialized Python future. Keep ordinary Python data in the kernel and large outputs outside model context.
A kernel failure must produce an explicit restore/loss result; never silently replay arbitrary cells or repeat unsafe external side effects.
Process isolation is not a security sandbox. Preserve existing permission behavior, secret masking, and Loki checks.

Use Prime Agent as a source reference, not as a replacement for Ultron's behavior:

- Repository: <https://github.com/PrimeIntellect-ai/prime-agent>.
- Inspected revision: `e6ebebebf751bdbdf78935b34a7053977fe863de`.
- Relevant paths: `prime-agent-runtime/src/rlm/repl.py`, its `repl.md` protocol, and `crates/pa-core/src/kernel/`.
- Its current evaluator is a custom CPython subprocess with a persistent namespace, top-level await, and one persistent asyncio loop.
- Nine focused upstream subprocess tests passed during investigation: values, output capture, await, background tasks, interruption, tracebacks,
  syntax errors, host callbacks, and shutdown. This is reference evidence only, not Ultron Rust acceptance.
- Snapshot tests were not run because the test interpreter lacked `dill`. Do not infer arbitrary-object recovery from those results.

Autolith was inspected at `fd20263e61ac8beda63dc33b4e5b24832811f97d` in <https://github.com/lambda-symbolics/autolith>.
Its live Common Lisp application and SBCL workers are a different architecture. Lisp and a Rust-language REPL are not planned for this rewrite.

## Compatibility decisions

Decided 2026-10-10 (decisions.tsv): built-in extensions are rewritten in Rust; third-party JS/TS extension loading is removed and replaced by an
out-of-process extension protocol (JSON, language-neutral, MCP-like); the TUI and every other surface are rewritten in Rust. Still open: browser and
Cloudflare targets.

### Background (before the decision)

The current extension loader executes TypeScript/JavaScript, including custom providers and terminal components.
Reimplementing built-in extensions in Rust is part of the rewrite. Existing third-party JS extensions do not become Rust extensions automatically.
A native replacement API, an optional language bridge, and removal of JS extension compatibility have different product consequences.
The user must approve compatibility removals before they happen. A permanent bridge is not assumed.
These choices do not block the durable storage and session port.

The repository also contains browser-facing APIs. Rust can expose a protocol or compile suitable components to WebAssembly,
but browser support cannot be counted as covered by a native executable alone.
Cloudflare-specific Durable adapters likewise need an explicit target decision before claiming full upstream platform parity.

## Product goals (2026-10-11)

Core principles stay intact: RLM-first, the model's only tool is the `rlm` Python REPL, every capability is a kernel API, the host only
enforces guarantees. Durability, sandboxing (applied to host operations, not tools) and the TUI serve that core.


- Performance: everything that can be Rust is Rust; paint never waits on network or disk.
- Ultron Durable: an Ultron that never dies and works on many things at once, modeled on Pi Durable. Sessions, tasks, children and
  queues are durable records; the server supervises workers and restarts them; kernels and providers recover; nothing acknowledged is lost.
- Sandboxing: shell commands and file writes run with real isolation, not only Loki checks.
- TUI quality of life inspired by omp (oh-my-pi), reference at ../omp-reference-src/oh-my-pi.

## RLM-first Rust architecture (2026-10-10)

The model has one tool, the persistent Python REPL. Every feature is a kernel API in Python backed by a Rust host operation; the Rust host
enforces guarantees (permissions, budgets, Loki, secret masking, wake-ups) and owns durable state. No native model tools, no host-driven chat loops
as the primary path.

Crates (one owned area each; dependencies point downward; adopted from prime-agent's layering, adapted to RLM-first):

| Crate | Owns |
| --- | --- |
| `chord-delta`, `durable` | Document deltas; Pi Durable storage and sessions |
| `session-file` | Pi session JSONL (users' existing sessions) |
| `ai` | Message model, providers, streaming, model metadata |
| `kernel` | Python kernel supervisor and wire protocol |
| `host` | Host operations the kernel API calls (bash, files, edits, handles, masking, Loki hook) |
| `config` | Agent dir, settings, auth, trust |
| `mcp` | MCP client and server management, reached through the kernel API |
| `tui` | Terminal rendering, input, components; renders protocol types, never links the engine |
| `inventory` | Generated feature inventory and its staleness check |
| next: `agent`, `runtime`, `server`, `cli` | Agent loop with the single REPL tool and RLM inference/delegation; server/client protocol; composition root |

Extensions: built-ins become Rust; third-party extensions are separate processes speaking a JSON-RPC protocol over stdio (MCP framing,
`initialize` handshake, capabilities). They can register kernel-API functions and slash commands, observe and veto lifecycle events, and contribute
declarative UI (status lines, panels, selectors) that the Rust TUI renders. They do not run code inside the host or the kernel.

Parity method (from prime-agent): differential evidence against the TypeScript implementation for every user-visible surface: recorded fixtures,
byte-compared protocol traffic, and TUI frame diffs in tmux; paint never waits on network or disk.

Runtime requirements learned in this session: waits on child work happen in the host, not in a REPL cell; a root may open more than one REPL lane;
a cell outliving its tool call can be detached or cancelled instead of blocking the lane.

## Waves

1. Done: kernel supervisor, session file format, inventory.
2. Done: providers (Anthropic, OpenAI completions/responses), Durable session layer, TUI core, MCP stdio client, config, host bash/jobs. 439 tests; parity 414/470.
3. Done: TUI editing/input/keybindings/widgets/terminal, host masking/images/jobs journal, MCP runtime, more providers, protocol/server/client. 517 tests; parity 492/548.
4. Done: agent loop, rlm.infer/rlm.load host, TUI Editor and Markdown (marked lexer), Loki core (Rust binary), `ultron` CLI args/help/version/list-models, system prompt/resources, compaction, themes. 608 tests; parity 590/646.
5. Done: `ultron -p` / `--mode json` end to end (agent turn, rlm cells in the persistent kernel, session file; output equal to TS against a mock provider), durable task supervisor/reducer/effect gate, interactive conversation components, TUI autocomplete/LaTeX/theme registry, autoreview/review deterministic core, telemetry and bug reports. Resuming after the Ultron runtime went away: rlm.map, delegation, Loki hook/rule packs, provider OAuth/images.
6. Running: session format 4 and continue/resume/fork, interactive mode loop, RPC mode, out-of-process extension protocol, code skills/memory/schedules/goals host ops, A01–A56 acceptance evidence in Rust.
7. Running: session server/worker/client (the shipped `ultron` path: native-command.ts -> server + session worker + experimental client-tui), Loki and file-hook guards in the host, CLI subcommands (auth, config/package, usage, migrate, export, setup, mcp), remaining providers and the Models runtime. The interactive worker was redirected from modes/interactive to experimental/client-tui, which is what users see.
8. Then: autoreview orchestration, Claude Code integration (`ultron claude|mcp|hook|watch|guide`), runtime.py wrappers for the new lane/collect APIs, release packaging without Node, cutover.

Done since: delegation (host-side waits, extra lanes, detach/cancel), rlm.map/frames, provider OAuth/images/models.json; all providers
reconciled in one crates/ai; daemon architecture (coordinator, server generations, workers, client runtime) with per-project instances
and `ultron durable`; crates/session-core as the one session path for print/json/rpc/TUI over the daemon; format-4 journals and named
sessions; client-tui TUI with QoL batches 1 and 2; Pi Durable scheduler; kernel continuity and sandboxed kernels; kernel APIs;
extensions protocol; Loki in Rust; CLI subcommands. 1325 tests; parity 1310/1364 (remaining: A01-A56 rows).

Running (2026-10-11): durable layer 2 (inbox, cell/generation tasks, one journal, swarm items), durable layer 3 (proactive restart,
storage lock, watchdogs, reconnect, signals), acceptance round 2, remaining TUI commands and overlay wiring, Claude Code integration,
autoreview orchestration, Hindsight backend, flaky-test root causes. Then: release packaging without Node and cutover.
Session naming (user decisions): -n/--name, /name, -r <name>, -r lists only the current directory, automatic names from one small background model call with a first-prompt fallback.

Python split: [docs/rust-rewrite/python-split.tsv](docs/rust-rewrite/python-split.tsv) lists each Python module as `rust` (heavy logic moves to host operations) or `python` (the REPL and thin model-facing APIs).
3. Next: agent loop + RLM runtime (infer/map/spawn/agents/budgets on durable tasks), `ultron` CLI print/JSON/RPC modes, server/client/daemon,
   Loki engine, interactive mode screens on the Rust TUI, extension protocol host, built-in extensions, autoreview/review/memory/skills/schedules,
   remaining providers, release packaging, A01–A56 acceptance in Rust.

## Why upstream Durable is a separate workstream

Ultron's current `packages/durable` exports storage and record contracts only.
The fetched upstream package also implements sessions, transactional documents, a scheduler, generation, tools, compaction, submissions, and observation.
Porting only Ultron's eight durable source files would omit most of the requested Pi Durable runtime.

Both versions declare SQLite schema version 1, but their schemas differ: upstream adds conversation ownership columns,
submission status and indexes, and task states such as `waiting` and `completing`.
The Rust implementation must not open a file based solely on that shared version number.
Identify each source format and test explicit import/conversion on copied fixtures before supporting existing databases.
Never test conversion against a live session file.

## Work rules

- Work only in the `rust-rewrite` worktree. Leave the existing checkout and installation alone.
- Start with source contracts and one runnable vertical slice; do not create empty crates or stub commands to suggest coverage.
- Reuse the current Python runtime where it meets the contract. Change its host boundary only with subprocess tests.
- Pin direct Rust dependencies to exact versions and review `Cargo.lock`. Retain licenses for translated or reused code.
- Give parallel implementation work separate worktrees and explicit ownership. Integrate and verify each unit before expanding it.
- Do not modify acceptance expectations, disable guards, or weaken tests merely to make the rewrite pass.
- No commits, pushes, publishing, paid model calls, or changes to live session data without authorization.

## Progress checklist

- [x] Create isolated `rust-rewrite` branch/worktree.
- [x] Fetch and pin upstream Pi Durable and its Chord/AI references.
- [x] Record the package census and all A01–A56 reference evidence.
- [x] Decide Rust host with Python REPL.
- [ ] Phase 1: executable verification baseline and feature ledger.
  - [x] Rust workspace, parity runner (`scripts/rust-parity.mjs`) and ledger (`docs/rust-rewrite/ledger.tsv`) that fail on missing evidence.
  - [x] Repeatable upstream fixture generation for Chord delta.
  - [x] Worktree `node_modules`; `npm run check` clean in this worktree.
  - [ ] Feature inventory per package (the `INVENTORY-*` ledger rows).
- [x] Chord delta op layer: validate, apply, applyImmutable(Batches), wire encoder/decoder (`crates/chord-delta`, CHORD-DELTA-01..05 verified; tracker/draft not ported).
- [x] Phase 2: Rust Durable storage and document semantics.
  - [x] Storage contract, typed records, `MemoryStorage`; upstream conformance replay (`crates/durable`, DURABLE-STORAGE-MEM-01..27).
  - [x] SQLite backend in upstream format 1, replaying the fixture directly and across reopen, with file-row identity (DURABLE-STORAGE-SQL/REOPEN, DURABLE-SQLITE-01..13).
  - [x] Ultron's own durable SQLite format: no product dependents, so no stored data to import (decisions.tsv). The live session format is the Pi JSONL of `core/session-manager.ts`.
  - [x] JSONL backend in upstream format 1: sidecar/main-marker ordering, torn and unconfirmed tails, reclamation, poisoning, fsync ordering (DURABLE-STORAGE-JSONL/JSONL-REOPEN, DURABLE-JSONL-01..24).
- [ ] Phase 3: Rust Durable sessions, scheduler, and harness.
- [ ] Phase 4: Rust providers and authentication.
- [ ] Phase 5: Ultron runtime and Python kernel integration.
- [ ] Phase 6: application, protocols, terminal, and compatibility surfaces.
- [ ] Phase 7: full parity audit and local release artifact.

The first implementation unit is Chord document operation decoding/replay with TypeScript-generated fixtures,
followed by Durable storage using the same document semantics. Its completion does not imply the full Chord composition runtime is ported.

## Execution order

Each phase ends with runnable evidence. Crates are added when their implementation starts, not as empty placeholders.
The provisional ownership boundaries are durable storage/session, provider transport, runtime, and the application surfaces;
crate names and exact type signatures follow the first working vertical slice.

### 1. Establish the verification baseline

- Build a feature ledger from public exports, CLI flags/commands, settings, provider registrations, extension APIs, protocols, and behavioral tests.
- Expand the A01–A56 baseline with product behaviors not represented by those rows. No unlisted feature is implicitly out of scope.
- Create a Rust workspace and a verification runner that fails on missing, skipped, or unverified required evidence.
- Capture deterministic outputs from the pinned TypeScript code using the faux provider and fixture-only storage.
- Preserve upstream copyright/license attribution for translated code and fixtures.
- Classify each behavior as reference-only, implemented/unverified, verified, or blocked; record the executing Rust test and its observed result.
  A status field alone is never verification. Missing evidence keeps the full-product gate failing.
- Establish existing `npm run check`, `./test.sh`, and `npm run test:acceptance` results in the isolated worktree.
  Use a real worktree `node_modules` directory with per-entry links; workspace packages resolve to the worktree itself.

Gate: baseline fixture generation is repeatable; the Rust product parity runner fails because its implementation is missing.
Missing parity must stay red even if foundational unit tests pass.

### 2. Port Pi Durable storage and document semantics

- Model distinct conversation, entry, submission, task, document, and sequence identities with validated Rust types.
- Port Chord delta semantics needed for documents: base/delta replay, paths, operation encoding, and historical reads.
- Implement the same storage contract across memory, SQLite, and JSONL, including atomic failure behavior, queries, pagination, and ID allocation.
- Cover document retirement, current-only versus rewindable history, ownership, forks, and corruption handling.
- Exercise JSONL sidecar/main-marker ordering, partial tails, reopen, reclamation, and durability options.
- Test both pinned source database formats without conflating their schema versions.

Gate: shared storage conformance cases pass on each Rust backend; kill/reopen cases preserve acknowledged commits according to the configured durability contract.

### 3. Port Pi Durable sessions and harness

- Serialize atomic commits and publish changes only after storage acknowledges them.
- Port submission deduplication, queue modes, context resets/edits, conversation fork/copy semantics, and document observation.
- Port task phases/checkpoints, child ownership, joins, abort propagation, terminal results, scheduler recovery, and close/failure behavior.
- Port registry precedence, hooks/wrappers, generation, tool results, nested tools, output streaming, compaction, usage, and events.
- Port filesystem/shell tools, output limits, image input, and execution-environment behavior with platform-specific tests.

Example recovery contract: submit input, commit tool intent, kill the process during execution, reopen the same fixture.
A replay-safe tool can rerun; an unsafe tool must produce the upstream interrupted result rather than repeat the side effect.
Cancelling a waiter must not cancel the durable work it observes.

Gate: upstream harness scenarios pass with deterministic providers, including restart, cancellation races, storage failure, and nested-tool recovery.

### 4. Port model/provider access

- Implement the message/content/usage/error model and validated tool schemas.
- Port provider wire protocols and model metadata, then provider-specific authentication and session behavior.
- Preserve streaming ordering, partial tool arguments, retry classification, cancellation, proxy configuration, image handling, and usage accounting.
- Keep replayable request/response fixtures; use live providers only with authorization.

Gate: offline protocol fixtures pass for every inventoried provider; a real provider is not needed for ordinary tests.

### 5. Port the Ultron runtime onto Rust Durable

- Route root, delegated, typed-agent, and workflow work through one durable task service.
- Port bounded context handles, frames, model pinning, budgets, events, cancellation, background jobs, and completion evidence.
- Integrate persistent Python kernels through the Rust supervisor: namespace retention, top-level await, last-expression results, host requests,
  background tasks, and output attribution. Separate protocol traffic from stdout/stderr, including raw writes and subprocess output.
- Port process-tree ownership, interruption/escalation, kernel restart/pooling, and asynchronous tool/MCP delivery.
- Verify selective namespace snapshot/restore and report skipped/lost values. Restore application tasks from durable records, not Python snapshots.
- Exercise terminal detach/reattach separately from process-crash recovery; a still-running kernel is not evidence of successful restore.
- Port context operations, memory/refinement, code skills, schedules/goals, worktrees, review/autoreview, and Claude integration.
- Preserve Loki/secret masking and project instructions. Do not disable guards to make the port pass.

Gate: Rust evidence covers all A01–A56 behaviors, including lost-response replay, detached work, cancellation, restore, and input/output bounds.
A mocked unit test cannot substitute for a required process-level test.

### 6. Port application and terminal surfaces

- Port native session import/export, settings/trust/auth storage, package/extension management, and migrations.
- Port server/client messaging, discovery, session workers, daemon ownership, and RPC/JSON modes.
- Port terminal input, keybindings, rendering, images, themes, overlays, session selection, and interactive commands.
- Port telemetry and operational commands without leaking credentials or using the live default state directories.
- Implement the approved extension contract and any required browser/platform surface.

Gate: CLI/RPC behavioral comparisons and actual terminal interactions pass against isolated installations; restart/reconnect and concurrent settings writers preserve state.

### 7. Cut over and package

- Audit the feature ledger for uncovered or stubbed paths, unsupported fallbacks, and hidden Node dependencies.
- Run full Rust checks, legacy reference checks, parity cases, and the release secret scan.
- Verify build/install/uninstall/update behavior on each supported target before declaring platform parity.
- Remove the superseded implementation only after the Rust path passes and any feature removal is approved.
- Produce a local release artifact. Do not publish, push, or alter the installed Ultron without permission.

Gate: the complete verified ledger, a standalone release artifact, and the repeatable verification command all agree on what is implemented.

## Verification commands

Once the Rust workspace exists, every implemented unit must pass:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Test names, fixtures, and a subprocess runner must tie those checks to observable behavior rather than matching implementation constants.
Phase 1 must add a separate full-product parity command; ordinary `cargo test` success alone does not prove the rewrite is complete.

Retained code and runtime integration also require the existing repository gates:

```sh
npm run check
./test.sh
npm run test:acceptance
```

Inspect the full check output and fix errors, warnings, and infos. A01–A56 must all pass in the reference suite,
and the Rust parity runner must execute Rust-backed evidence for the same behaviors.
Never relabel the TypeScript report as Rust evidence. Review any changed acceptance evidence before refreshing its lock.

Record decisions in [decisions.tsv](docs/rust-rewrite/decisions.tsv). Keep test output and fixture provenance with each phase's evidence.
Before release, apply the existing release checklist, including `npm run scan:secrets`, and add Rust artifact checks.

## Checkpoint (2026-10-11, all work stopped)

Verified in this worktree at the checkpoint:
- `cargo fmt --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings`: clean.
- `cargo test --workspace --locked`: 1455 tests; the last full run passed (exit 0). One test fails only under heavy machine load
  (`cli_rpc_steer_and_follow_up_while_running_match_typescript`; passes alone 3/3); two scheduler tests behave the same way
  (decisions.tsv "flaky" rows). Root causes not yet fixed.
- `npm run rust:parity`: 1439 of 1493 ledger rows verified. Missing rows are A01-A56 acceptance behaviors (A02, A10, A27 verified).
- `npm run check`: clean. `./test.sh` and `npm run test:acceptance` were not rerun after `runtime.py` changed; 44 + 39 focused
  TypeScript kernel tests passed against the new `runtime.py`.

Working through the Rust binary: `ultron` interactive (client-tui parity plus omp-inspired QoL), `-p`, `--mode json`, `--mode rpc`,
all as clients of the Rust session daemon (coordinator, server generations, supervised workers) on one session path
(`crates/session-core`); format-4 journals readable by the TypeScript product; named sessions with model-made titles; per-project
instances and `ultron durable`; sandboxed kernels with crash continuity; kernel APIs, delegation lanes, rlm.infer/map, extensions
(ext.* functions, never model tools); Hindsight memory shared by all instances; Loki in Rust; most subcommands; all providers.

Built and tested as libraries, not yet the product path: the durable conversation runtime (`crates/harness::chat`: inbox, durable
cells and generation, children as tasks, persistent children, peer messaging, task board), the Pi Durable scheduler.

Stopped mid-flight (partial, unmerged, in ../ultron-rust-wt/<name>; resume from those trees, merge base in brackets):
- accept2 [_base13]: acceptance round 2 (memory rows A16-A18, A28-A30 have passing `_ported_part` tests; clippy clean).
- tuicmds [_base13]: remaining TUI commands and overlay wiring.
- claude [_base13]: `ultron claude|mcp|hook|watch|guide` port.
- review2 [_base13]: autoreview orchestration.
- durable-p2 [_base13]: proactive worker restart, storage lock, watchdogs, reconnect, signals.
- cleanup [_base10]: flaky-test root causes, Loki shell-writes dedupe.
Tools: ../ultron-rust-wt/tools/{merge.py,gates.sh,integrate.py,auto_rows.py,set_arow.py}; briefs in ../ultron-rust-wt/BRIEF.md.

Next, in order: finish and merge the stopped workers; make `harness::chat` the session path in `crates/session-core`; fix the
load-only test flakes; remaining acceptance rows; release packaging without Node; cutover.

Needs the user: (1) the Claude launcher passes `--dangerously-skip-permissions` to `claude` (kept for parity); (2) approval before
the TypeScript product is removed at cutover.

## Ultron Durable build list (2026-10-11, from the Pi Durable and omp surveys)

Gap found: Rust has Pi Durable's storage and session layer but none of its harness; print mode runs an in-memory loop.

- P0 `harness`: scheduler (open reconcile, resume, reservation, faults, handover, definition blocking), TaskRuntime, structured
  concurrency (waiting failFast/allSettled, completing holds, background, abandonOnRestart), close/failure, kill-and-reopen test. Running.
- P1 `harness`: submissions and inbox (steer/followUp/write), durable REPL-cell task (unsafe by default: interrupted + partial on
  reopen), durable generation task (pinned resend, retry checkpoints, overflow compaction) as the product path, delegation children as
  tasks (one journal), many conversations under one harness.
- P2 `server`: worker supervision with automatic restart (TS only prints `ultron -c`), single-owner storage lock, coordinator
  handoff, heartbeats/watchdogs, client reconnect. Project-isolated instances plus the global durable instance (user decision).
- P3 `kernel`: snapshot/restore with HMAC signing, auto-restore, snapshot after each state-changing cell; report restored/lost names. Running.
- P4 `host`/`harness`: schedules, goals and shell jobs as durable tasks.
- Sandbox `sandbox`: bwrap -> unshare -> docker launcher for interactive shell/writes, doctor check, omp approval tiers and bash
  critical patterns. Running.
- TUI (omp-inspired): notifications, writer thread + scrollback/live split, status line segments, vim mode, typed paste and
  attachments, syntect highlighting, extra autocomplete (running); then /copy and rewind over the transcript, agent hub, model hub,
  plan mode review, btw panel.

## Swarm design items (from "On the Nature of the Swarm", Prime Intellect, read 2026-10-11)

Multi-agent work mainly adds context capacity. Adopted for Ultron Durable, all as kernel APIs over durable tasks (RLM-first):
- Persistent children: `rlm.spawn(..., keep=True)` children stay alive (context, files, processes) as durable tasks that survive restarts;
  follow-up questions go to them (`rlm.ask(child, question)`), so the caller's context grows only by question and answer.
- Peer messaging and routing: any agent can message any other in the session (family messages, A41, made durable), plus a small
  registry of what each agent holds (topics, files, recent work) so questions route to the agent with the context instead of via the root.
- Shared world: a durable task board and notes document per session that agents read and write as the source of truth.
- Defaults: delegate after the question is known; summaries point to handles and files; start flat and add hierarchy only where
  coordination cost justifies it.
