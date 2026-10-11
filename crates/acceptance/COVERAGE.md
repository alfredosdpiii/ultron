# Acceptance A01-A56: Rust evidence

Each row of `acceptance/manifest.json` mapped to the Rust test that reproduces its TypeScript evidence, or to what
blocks it. A row is **verified** only when every evidence test the manifest names has a Rust counterpart that runs
the Rust implementation (process-level where the TypeScript test is). **Partial** rows have a passing Rust test for
the ported part and stay unverified in `docs/rust-rewrite/ledger.tsv`. **Blocked** rows have no Rust test: the
subsystem does not exist in Rust yet, and no passing stand-in is written.

How the tests run: `tests/*.rs` drive the real `packages/coding-agent/src/ultron/rlm/runtime.py` through
`crates/kernel`, the host operations of `crates/host`, `crates/inference`, the task supervisor of `crates/harness`,
and the `ultron` binary. The binary is `crates/cli/src/main.rs` compiled by this package as `ultron-acceptance`
(same source and library code, same docs/runtime.py paths), so tests can run it as a subprocess
(`CARGO_BIN_EXE_ultron-acceptance`). End-to-end tests run `ultron -p --no-session` in private temp dirs
(`ULTRON_CODING_AGENT_DIR`, `ULTRON_SERVER_DIR`, `HOME`) against a scripted OpenAI-compatible provider on 127.0.0.1
(`tests/common`), which answers root turns and inference frames and records every request body. The TypeScript
evidence uses RPC mode; the Rust binary has print mode only, which runs the same agent turn.

Mutation slice (each seeded, run, reverted; all killed): kernel credential filter without `TOKEN` (A10), without the
worker-control prefix rule (A10); tool-result truncation disabled (A02); unknown host requests answered instead of
refused (A27); task journal lets a later terminal result overwrite the first (A05); supervisor cancellation that
does not abort the task's work (A05); frame contract never re-asked (A48).

| Row | Status | Rust test | Blocker / notes |
| --- | --- | --- | --- |
| A01 | blocked | | Definition registry (`agents.register`/`agents.invoke`), lane execution of definitions, and `workflows.run` are not in Rust. |
| A02 | verified | `acceptance_a02_large_input_stays_in_the_kernel_and_the_model_sees_only_bounded_results` | Kernel + `host::cell` tool text over a 9 MB CSV; then the `ultron` binary: the follow-up request carries the aggregate and a bounded result, never the sentinel row. |
| A03 | blocked | | Predict strategy and its adapter (`predict-adapter.ts`), `maxRepairs` on definitions, rlm-strategy typed failure (definition registry). `rlm.infer` repairs exist (`crates/inference`) but are not this row's evidence. |
| A04 | blocked | | Lane runtime (child lanes with their own model; `setModel`) is not in Rust; `crates/harness` supervisor only admits. |
| A05 | partial | `acceptance_a05_cancellation_kills_owned_work_and_rejects_late_success_ported_part` | All five cases of `ultron-a05-cancellation-process-tree.test.ts` ported (kernel tree kill incl. setsid grandchild, shutdown kill, task cancel kills the lane kernel tree and keeps the durable cancelled result across reopen, late success never overwrites, 30-iteration race). Missing from `ultron-native-host.test.ts`: task timeouts, `background.*` jobs, deterministic adapters, workflows, reviewer verdict (`childResult`); its admission/idempotency/reopen cases are replayed in `crates/harness/tests/supervisor_fixture.rs`. |
| A06 | partial | `acceptance_a06_output_and_process_limits_hold_ported_part` | Ported: 50 MB flood bounded, raw fd writes bounded and not protocol, runaway cell interrupted, interrupt and shutdown kill child and grandchild; long-session and raw-output files are ported in `crates/kernel/tests/process.rs`. Missing: artifact size/quota (`artifacts.*`), task `timeout_ms`, root wall deadline and admitted-task cap (usage ledger). |
| A07 | blocked | | Ultron session values, lane resume/fork (`harness/runtime`), refinements anchored to branches (local services). |
| A08 | blocked | | Kernel snapshot/restore requests and the host-side HMAC signing (`snapshot-auth.ts`) are not in `crates/kernel`. |
| A09 | blocked | | Usage ledger (`usage.ts`): task-count admission, wall deadline, reservations. |
| A10 | verified | `acceptance_a10_kernel_gets_no_ambient_credentials_or_unauthorized_host_capability` | Credentials and worker-control variables planted in the `ultron` process env never reach Python or any request; an ungranted host request fails in Python; `ULTRON_RLM_ENV_ALLOW` passes a named variable. |
| A11 | blocked | | Grants (`grants.ts`). |
| A12 | blocked | | Workflow graph contract (definition registry). |
| A13 | blocked | | Workflow dependencies over the task journal. |
| A14 | blocked | | Release gates, progress, schedules, grants. |
| A15 | blocked | | Release gate (`gates.compare`). |
| A16 | blocked | | Memory service (Hindsight adapter). |
| A17 | blocked | | Memory service. |
| A18 | blocked | | Memory service. |
| A19 | blocked | | Grants and refinement services. |
| A20 | blocked | | Refinement service. |
| A21 | blocked | | Deterministic definitions, usage ledger, owner-fenced task store recovery. |
| A22 | blocked | | RPC mode and the session worker/daemon (detach, reattach, orphan grace). |
| A23 | blocked | | Schedules and goals. |
| A24 | blocked | | Accounting reconciliation (`reconcile.ts`, progress, usage). |
| A25 | blocked | | Pi session migration import/round trip (`migration.ts`) and native RPC. |
| A26 | blocked | | No Rust instrument lock or mutation runner yet; the mutation slice above is recorded by hand. |
| A27 | verified | `acceptance_a27_rlm_completes_a_self_contained_task_without_host_services` | Recording and rejecting kernels, the Rust host operations behind a recorder with an empty task supervisor, and the `ultron` binary running the task as `rlm` cells; `memory.prepare` and `refinements.list` fail explicitly. |
| A28 | blocked | | Memory retrieval decisions. |
| A29 | blocked | | Memory inspection. |
| A30 | blocked | | Memory forget propagation. |
| A31 | blocked | | Skill selection (`skills.ts`). |
| A32 | blocked | | Skill proposals (local services). |
| A33 | blocked | | Experiment history (local services, memory). |
| A34 | blocked | | Its outage scenario needs memory, refinement, modules and workflows, none of which exist in Rust. |
| A35 | blocked | | Deterministic/predict/rlm strategy parity (definition registry). |
| A36 | blocked | | Retained instances (`instances.ts`). |
| A37 | partial | `acceptance_a37_kernel_preview_is_bounded_and_keeps_full_values_ported_part` | All six kernel cases ported with literal expectations. Missing: artifact range reads (`artifacts.put`/`artifacts.read`). |
| A38 | blocked | | `background.*` jobs and per-lane kernels (lane runtime); the supervisor's child-lane visibility exists but the evidence needs both. |
| A39 | blocked | | RPC mode; the metered live suite also needs separate authorization. |
| A40 | blocked | | Retained instances. |
| A41 | blocked | | Family messages (`family.ts`). |
| A42 | blocked | | Family intervention, progress, lanes. |
| A43 | blocked | | Kernel pool (eviction, snapshot-before-evict) and artifact reclamation. |
| A44 | blocked | | Progress and reassessment (`progress.ts`). |
| A45 | blocked | | Usage ledger capacity under saturated recursion. |
| A46 | blocked | | RPC mode and every subsystem above (full demonstration). |
| A47 | blocked | | `rlm.map`, frame tasks journaled as `rlm-frame@1`, `rlm.frames`, RPC `inspect`. |
| A48 | partial | `acceptance_a48_frame_repair_and_exhaustion_end_to_end_ported_part` | End-to-end through the `ultron` binary: repair returns the exact count, exhaustion is a falsy `Incomplete` (`contract_unmet`, 2 calls, last outputs) and the root turn ends with `DONE`; the host-level cases are in `crates/inference/tests/units.rs`. Missing: frame traces (`rlm.frames` by trace id). |
| A49 | blocked | | `rlm.map` (shared budget, tranches across siblings) and nested frames (depth > 1). |
| A50 | blocked | | `rlm.map` cancellation and root-turn abort bridged to the kernel (print mode does not bridge SIGINT; no RPC abort). |
| A51 | blocked | | Context control (collapse on return), `agents.result`, RPC. |
| A52 | blocked | | `ctx.forget`/pins (context control), extension event bus, compaction of notes and pins in the live session. |
| A53 | blocked | | Code skills (`code-skills.ts`). |
| A54 | blocked | | Code-skill reuse through RPC. |
| A55 | blocked | | Agent classes need the definition registry and `agents.invoke`, plus kernel snapshots. |
| A56 | blocked | | Async completion events, RPC, hints, extension/MCP tool calls from the REPL. Host-owned shell jobs exist in `crates/host` (and its tests), but the row's evidence centres on delivery without polling. |
