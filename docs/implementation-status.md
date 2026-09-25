# Ultron implementation status

## Native and tested

- Ultron is a separate fork with package and command identity `ultron`.
- Ultron resolves its own configuration under `~/.ultron/agent`, while preserving project-local `.pi` resource compatibility.
- The durable session worker owns the native `ipython` tool.
- Python state persists across calls, output is bounded, and Python failures are returned as tool failures.
- RLM host requests are routed through the session worker.
- The worker has native typed-agent definitions for `identity@1`, `security-reviewer@1`, `correctness-reviewer@1`, and `tests-reviewer@1`.
- Task admission, task state, task results, cancellation requests, workflow dependency ordering, and task records use one serialized session value document when the native host is wired into the worker.
- Agent output is checked against the reviewer output contract. Failed checks return `verification: "unverified"`.
- Restarted admitted or running task records become `interrupted`; Ultron does not replay them automatically.
- Optional controls are reported disabled. They do not block routine work.
- The source-level checks, native RLM tests, native host tests, and offline build pass.
- The old host services and tests are copied to `packages/ultron-runtime` as a compatibility/reference package. The CLI does not import them.

## Not complete

This is an implementation status file, not a claim that the full architecture plan is complete.

- Hindsight and Jev are connected when their configuration is present. Without a Jev key, memory retention fails closed and recall is skipped.
- Refinements, artifacts, and experiments are connected to the native RLM worker. `createCodingAgentHarness` in `packages/coding-agent/src/experimental/session-worker.ts` creates `createWorkerServices`, passes it to `NativeRlmHost`, and the RLM Python host-request bridge reaches those handlers. `createWorkerServices` persists the data through the session value store.
- Explicit background jobs now use the native task journal and `background-job@1`. They support start, list, inspect, result, stop, idempotency keys, cancellation, bounded prompts, and durable interruption on owner restart without automatic replay.
- The native task journal now has restart recovery, idempotency-key conflict checks, commit-before-publish transitions, and terminal cancellation handling. It uses one session value document rather than a separate task database.
- Cancellation is cooperative through the child lane. It does not undo effects already performed by a model or tool.
- Native definitions support durable runtime registration with strict JSON Schema validation, immutable ID/version hashes, predict adapters, deterministic adapters, and bounded output repair.
- The packaged CLI acceptance path now completes an API-backed prompt through the local `cliproxyapi/gpt-5.6-sol` provider. The response was verified in both text and JSON output modes.
- The packed npm consumer acceptance test now installs the published tarballs into an isolated consumer and completes a native prompt against a deterministic local OpenAI-compatible provider. It verifies shipped worker entrypoints, runtime assets, request routing, and native CLI output.
- The A01-A46 review is recorded in `docs/a01-a46-acceptance.md`. Rows are explicitly marked passed, unverified, blocked, or failed. The file is an acceptance instrument, not a claim that all rows pass.
- Every agent run goes through the native RLM worker. `--help`, `--list-models`, and `--export` do not run the agent and keep Pi's handlers. `--export` renders native session files with Pi's HTML exporter.
- `--mode rpc` runs natively (`src/experimental/rpc-native.ts`) and speaks Pi's JSONL protocol. Pi's `RpcClient` drives it in `test/ultron-native-rpc.test.ts`. Differences from Pi: model objects carry `provider`, `id`, `name`, and `reasoning` only; `get_tree` returns the active branch as a single path; entries use the native entry format; extensions run headless, so no `extension_ui_request` events are emitted.
- Worker-side session settings (name, queue modes, auto-compaction, auto-retry), user `bash`, and command listing go through the `ultron.session-control` service.
- Extension slash commands reach the native TUI and RPC. Before, the worker never received them.
- `--name` names a new native Session and renames an existing one selected with `--session`, `--session-id`, `--continue`, or `--resume`. Renaming a Session that a worker currently holds is rejected.
- Pi-format session files are rejected. Ultron sessions use the native JSONL format.

## Current evidence

```text
npm run check                         PASS
./test.sh                             PASS
  scripts                            16 passed
  agent                              934 passed, 1 skipped
  ai                                 1107 passed, 855 skipped
  chord                              282 passed
  client                             27 passed
  coding-agent                      2473 passed, 50 skipped
  durable                             77 passed
  evals                               55 passed
  protocol                           133 passed
  server                              44 passed
  telemetry                           15 passed
  session-backends/sqlite-node      105 passed
npm run build:offline                 PASS
ultron --version                      0.87.1
packed native CLI consumer            PASS
cliproxyapi/gpt-5.6-sol text smoke   PASS
cliproxyapi/gpt-5.6-sol JSON smoke    PASS

Focused native RLM local-services integration:
cd packages/coding-agent && node "$(git rev-parse --show-toplevel)/node_modules/vitest/dist/cli.js" --run test/ultron-rlm-tool.test.ts
  2 passed
```

These results prove the repository checks, isolated workspace suite, packed native CLI consumer path, packaged build, local provider path, native RLM local-services path, and native background task dispatch listed above. The focused tests execute Python through `RlmKernel`, send local-service requests through `NativeRlmHost`, verify session-backed results, and run a background task through the same durable task host. They do not prove statistical experiment conclusions, automatic refinement activation, or the A01-A46 rows marked unverified or blocked.
