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
(`tests/common`), which answers root turns and inference frames and records every request body. Where the
TypeScript evidence drives the CLI over RPC, the Rust test does too: `Install::rpc` runs `ultron --mode rpc` (server
generation plus session worker) through the Rust `RpcClient`. Module-level TypeScript tests that run over
`NativeRlmHost` use `tests/g3_support` (a `TaskHost` over the real `harness` `TaskSupervisor`: admission, idempotency,
parent links, subtree cancel, restart recovery; lanes are controlled fakes as in the TS tests). Typed-agent tests use `tests/agents_support`:
the TS `hostFixture` over the Rust typed-agent host (`delegation::agents`, its definition registry, task journal,
usage ledger and workflows), with lanes faked as the TS fixture fakes them (a script per prompt). Memory tests use
`tests/memory_support`: the TS `FakeHindsight` as the HTTP transport of the real Hindsight adapter
(`crates/hindsight`, copied unchanged from the main checkout), as the TS fake is `createHindsightBackend`'s `fetch`:
wire paths, bodies and status codes go through the adapter.

Hooks added outside this crate for these rows: `session-core/src/lane.rs` (the `rlm` tool description lists the
profile's code skills, TS `codeSkillsToolSection`; A54), `session-core/src/compose.rs` (`rlm.map` and frame-model
settings in the shipped session stack via `inference::Frames`, and frames aborted with their cell; A47-A50),
`inference/src/lib.rs` (an in-flight frame request ends when its signal aborts; A50).

Mutation slice (each seeded, run, reverted; all killed): kernel credential filter without `TOKEN` (A10), without the
worker-control prefix rule (A10); tool-result truncation disabled (A02); unknown host requests answered instead of
refused (A27); task journal lets a later terminal result overwrite the first (A05); supervisor cancellation that
does not abort the task's work (A05); frame contract never re-asked (A48).
Typed-agent slice (each seeded, run, reverted; all killed): admitted-task cap `>=` to `>`, idempotent retry without the
key lookup, root wall deadline not capping child deadlines (A09); when-route condition ignored (A12); cancellation
without the child cascade (A45); schema `minimum` ignored, predict attempts one short (A03); output not validated for
deterministic work (A35); dependent started without waiting for the terminal result (A13); missing admission not
reported (A24); artifact quota not enforced (A43); range read ignoring the offset (A37).
Second slice (all killed; equivalent mutants noted): snapshot HMAC comparison skipped (A08); grant expiry `>=` to `>`,
owner lineage ignored (A11); verifier steering allowed, inbox cap `>=` to `>` (A41); cancellation without aborting
the task's work, subtree cascade skipped (A42); frame repair never re-asks (A48); failing test activates the skill,
secret check never sensitive, unpublish skipped on rollback (A53; publishing on a failed test is equivalent, it
republishes the active version); code-skill listing dropped from the `rlm` tool description (A54); recall
`tags_match` exact to any, decision reuse kept after a mutation (A28); superseded claims not excluded (A17);
pending receipts read as stored, crash reopens as interrupted, forget exclusion removed (A18; removing only the
forgotten-set check is equivalent, the journal's withdrawn check excludes the same owned items).

Durable-work slice (each seeded, run, reverted; all killed): a claimed slot not due at once, a host-closed goal job
not treated as cut off, the goal run not written before its task is admitted, an admitted goal run not adopted, the
schedule tick not firing at exactly the due time, a schedule change not waking the tick (A23); every running job
interrupted on reopen, a lost job reported completed, released jobs stopped on drop (A56; killed by a hang: dropping
the host waits on a job its released follower no longer settles).

| Row | Status | Rust test | Blocker / notes |
| --- | --- | --- | --- |
| A01 | verified | 3 tests in `a01_invocation_parity.rs` | Direct (`agents.invoke`), graph (`workflows.run`) and a real runtime.py kernel cell (`await agents.invoke(...)`) over the Rust typed-agent host (`delegation::agents`): one validated result and journal record, identical failures for invalid output and input. Host replay against `native-host.ts`: `crates/delegation/tests/agents_fixture.rs`. |
| A02 | verified | `acceptance_a02_large_input_stays_in_the_kernel_and_the_model_sees_only_bounded_results` | Kernel + `host::cell` tool text over a 9 MB CSV; then the binary over RPC (as the TS test): the follow-up request carries the aggregate and a bounded result, never the sentinel row. |
| A03 | verified | 7 tests in `a03_bounded_repair.rs` | Both files ported (bounded predict repair, maxRepairs cap, typed failures, rlm never repairs; the production predict adapter `delegation::agents::predict_adapter`, whose prompts replay `predict-adapter.ts` in `crates/delegation/tests/predict_fixture.rs`). Divergence: a Rust adapter cannot return a non-JSON value (TS NaN); that case runs through the production adapter's unparsable-reply path. |
| A04 | verified | `acceptance_a04_an_explicit_child_model_reaches_only_the_child_lane_the_root_lanes_model_is_never_changed` | The child lane's model is set only when named (`LaneRun.model`); a malformed one is refused. `rlm.spawn(model=...)`: a malformed model is refused with "model must be provider/model", and the shipped session stack installs `Delegation::set_model_check(builtin_model_check())` (`session-core/src/compose.rs`), so an unknown model is refused instead of falling back (`crates/delegation/tests/spawn_model.rs`). |
| A05 | verified | `acceptance_a05_cancellation_kills_owned_work_and_rejects_late_success_ported_part`, 10 tests in `a05_native_host.rs` | All five cases of the process-tree file; all ten cases of `ultron-native-host.test.ts` over the Rust typed-agent host (`background.*`, deterministic `identity@1`, idempotent invokes, cancellation, parent cancellation before admission, timeouts, reopened journal, workflow validation, reviewer output). A TS lane result that is not a completed prompt at its tip is a lane-runner error in Rust (the run reports no assistant result). |
| A06 | verified | `acceptance_a06_output_and_process_limits_hold_ported_part`, `acceptance_a06_task_past_its_timeout_is_cancelled_with_a_durable_cancelled_result`, 4 tests in `a06_agent_limits.rs` | Artifact size/quota and the 1 MiB frame refusal from Python (`kernel_apis::artifacts`), the timeout through `agents.spawn` with a reopened journal, and the root wall deadline capping and then refusing admission (`delegation::usage`); long-session and raw-output files in `crates/kernel/tests/process.rs`. |
| A07 | blocked | | No Ultron session values, lane resume/fork in a task host, or local services (lessons, refinements). |
| A08 | verified | `acceptance_a08_snapshot_type_matrix_reports_every_unsupported_name_with_a_reason` (+5 in `a08_snapshot_restorability.rs`) | All six cases: type matrix, typed restore, startup restore, tampered body, unsigned/forged, failed startup. |
| A09 | verified | 7 tests in `a09_budget_admission.rs` | Both files ported over `delegation::usage` (the ledger replays `usage.ts` byte for byte in `crates/delegation/tests/usage_fixture.rs`): bursts on real threads, nested spawns from child lanes, idempotent retries, the root wall deadline, unknown usage. Divergence: the direct ledger race admits some 5 of 50 (TS: the first 5 by promise order); the retried key is one that won. |
| A10 | verified | `acceptance_a10_kernel_gets_no_ambient_credentials_or_unauthorized_host_capability` | Kernel-level, as the TS evidence. |
| A11 | verified | `acceptance_a11_a_check_is_a_pure_evaluation_that_reports_every_failing_binding` (+8 in `a11_grants.rs`) | All nine cases of `ultron-grants.test.ts` through `GrantsModule` over `g3_support`; `checkGrant` has no separate Rust function, so the pure case evaluates a stored document and checks the store is unchanged. |
| A12 | verified | 7 tests in `a12_graph_contract.rs` | `workflows.run` of the Rust typed-agent host: validation before effects, fan-in, routes, transitive skips, bound-input and admission failures. Ready nodes are admitted in node order (as TS), then run concurrently. Divergence: `worktree: true` workflow nodes are refused (worktree subagents run through `rlm.spawn`). |
| A13 | verified | 3 tests in `a13_admission_not_dependency.rs` | A dependent starts only after the dependency's terminal result is committed; `agents.spawn` returns admission only; a timed-out dependency skips its dependent for good. |
| A14 | blocked | | No release gates (progress, schedules, grants exist). |
| A15 | blocked | | No `gates.compare`. |
| A16 | partial | `acceptance_a16_skip_sends_nothing_and_secrets_are_refused_before_any_request_ported_part`, `acceptance_a16_scope_tags_pinned_through_consolidation_and_cross_scope_merge_refused_ported_part`, `acceptance_a16_live_recall_after_a_real_consolidation_keeps_exact_scope_tags_opt_in` (ignored) | Both default cases run through the real Hindsight adapter. The live case is ported behind the same switch as TS (`ULTRON_LIVE_HINDSIGHT=1`, `--ignored`) but no Rust live run is recorded (TS stands in `acceptance/demonstrations/a16-live-hindsight.json`). |
| A17 | verified | `acceptance_a17_corrected_claim_is_current_and_stale_claim_superseded_in_fresh_sessions`, `acceptance_a17_evidence_classes_stay_distinct_and_forged_journals_are_refused` | `MemoryService` through `hindsight::HindsightBackend` over the fake Hindsight transport. |
| A18 | verified | 4 tests in `a18_retention_forget.rs` | Same stack as A17; restart = a new service over the same journal. |
| A19 | blocked | | No local services or refinement service. |
| A20 | blocked | | No refinement service. |
| A21 | blocked | | No deterministic definitions or usage ledger. |
| A22 | verified | 12 tests in `a22_detached_work.rs` | Every case of `ultron-a22-detached-work.test.ts` over RPC and the daemon, with an `rlm.spawn` subagent as the detached work (no `background.*` client path): reattach after SIGTERM, a durable result collected by a later worker (the subagent journal is the session's `ultron.tasks/root`), idle cleanup, a root turn surviving client SIGTERM / stdin close / SIGKILL, explicit abort, hard-killed idle and busy clients, restart within the orphan grace. Beyond TS: a running child survives a worker SIGKILL (reattached, its pinned request resent, collected under the same id). |
| A23 | verified | 10 tests in `a23_schedules.rs`, 6 in `a23_durable_work.rs` | All ten TS cases: the module-level ones over `g3_support`; the goal, usage-budget and background-timer cases over the typed-agent host (`identity@1` through its definition registry and deterministic adapter, the usage ledger's admitted-task limit) with `kernel_apis::ScheduleTicker` on a manual clock as the timer. Beyond TS, kill/reopen evidence (a SIGKILLed child owns the files; the parent reopens through `kernel_apis::DurableWork`): a firing runs exactly once across a crash after the slot claim or after admission, slots missed while down coalesce and fire on their own, the session goal keeps its checks and runs on (interrupted run followed by the next; an admitted run adopted, not started twice), and the wiring harness drives schedules, goal and shell jobs through runtime.py across a hand-over. |
| A24 | verified | 2 tests in `a24_accounting_reconciliation.rs` | `delegation::reconcile` (replays `reconcile.ts` over the TS A24 records in `crates/delegation/tests/reconcile_fixture.rs`) over a mixed run through the Rust typed-agent host, usage ledger and the real `kernel_apis` progress and schedule modules, all in owner-fenced documents; crash recovery settles open work as interrupted/unknown; tampering is detected. |
| A25 | partial | 7 tests in `a25_migration.rs` | Backup/restore/tamper/unsafe paths, CLI usage, RPC event translator, native RPC (partial). Missing: `migrate import-pi`/`export-pi` (12 cases plus the CLI end to end), RPC `inspect`/`export_html` (`cli/src/rpc/mod.rs`), remote `get_commands`, TypeScript extension loading. |
| A26 | blocked | | Instrument lock and mutation runner exist only in Node (`scripts/acceptance-lock.mjs`). |
| A27 | verified | `acceptance_a27_rlm_completes_a_self_contained_task_without_host_services` | TS evidence uses the session-worker tool, not RPC; print mode is the counterpart. |
| A28 | verified | 5 tests in `a28_retrieval_decision.rs` | Same stack as A17. The TS gate also mutates its input; a Rust gate receives `&Value`, so that variant cannot be written. |
| A29 | partial | `acceptance_a29_why_shows_injected_evidence_without_a_new_search_or_other_scopes_ported_part` | No session-control inspector (`createSessionControl().inspect`) refusing searching/mutating requests. |
| A30 | partial | `acceptance_a30_forgotten_and_corrected_memories_leave_every_view_ported_part`, `acceptance_a30_a_lagging_backend_index_cannot_reinject_a_forgotten_memory`, `acceptance_a30_forgetting_persists_across_a_new_service_instance_on_the_same_store` | Missing the rendered `/memory list` view (`formatInspection`) and the `refinements.propose` refusal. |
| A31 | partial | 9 tests in `a31_skills.rs` | `skills.invoke` validates but never spawns (`kernel-apis/src/skills.rs`): blocks "invoke through the real host" and the spawned halves of the frontmatter and concurrency cases. |
| A32 | blocked | | No skill proposals through local services / refinement service. |
| A33 | blocked | | No experiment history (local services). |
| A34 | blocked | | Outage scenario needs release gate, usage ledger, refinements. |
| A35 | verified | 5 tests in `a35_strategy_parity.rs` | Deterministic, predict and rlm share admission, idempotency, timeout/cancel (the work's signal is aborted; only rlm lanes are aborted), usage reservations and journal transitions. |
| A36 | partial | `acceptance_a36_reset_clears_scratch_and_restores_bindings_while_declared_state_persists_ported_part` | Retained-instance cases need a task runtime running `rlm-child@1`/`identity@1` on named lanes with lane reuse (`InstancesModule` exists, nothing runs definitions). |
| A37 | verified | `acceptance_a37_kernel_preview_is_bounded_and_keeps_full_values_ported_part`, `acceptance_a37_range_reads_return_exact_byte_ranges_including_ranges_that_split_multibyte_characters` | Range reads of `kernel_apis::artifacts` (byte-exact `base64`, lossy `text`, bounded ranges); the service replays `local-services.ts` in `crates/kernel-apis/tests/artifacts.rs`. |
| A38 | partial | `acceptance_a38_child_lanes_receive_only_their_own_prompt_and_kernel_ported_part` | Over RPC: the child's requests carry only its prompt and its kernel is another process. Reviewer half needs `agents.invoke`; query-scope file needs `background.*` and `agents.*`. |
| A39 | blocked | | Live metered provider run; needs separate authorization. |
| A40 | partial | (shares `ultron-instances.test.ts` with A36) | Same blocker as A36. |
| A41 | verified | `acceptance_a41_derives_identity_from_real_task_lanes_and_steers_a_live_child_lane` (+12 in `a41_family.rs`) | All 13 cases; the native-host case through `g3_support`. |
| A42 | verified | `acceptance_a42_inspect_correct_and_stop_one_childs_subtree_while_the_sibling_continues_and_completes`, `acceptance_a42_stopping_a_finished_child_still_stops_its_live_descendants_and_leaves_siblings_alone` | Real supervisor with family and progress modules. |
| A43 | partial | 4 tests in `a43_resource_release.rs`, 2 tests in `a43_artifacts.rs` | Kernel pool cases and the artifact retention/quota cases (pinned checkpoint survives delete and reclaim; quota never evicts) ported. Missing: the session's REPL lanes do not use `KernelPool` (refuse past `max_lanes`), no `rlm.pool` stats (wired-pool test). |
| A44 | partial | 7 tests in `a44_progress.rs` | "Never verifies a completion claim without a passing verifier" needs `agents.register` and deterministic verifier runs. |
| A45 | verified | 2 tests in `a45_saturated_recursion.rs` | Saturated recursion returns immediate capacity failures (invoke and workflow nodes) and completes; cancelling a saturated parent cascades and releases every slot. |
| A46 | blocked | | Needs definition registry, usage ledger, release gates, refinements, memory wiring. |
| A47 | partial | 3 tests in `a47_a50_bounded_inference.rs` | Frames not journaled as `rlm-frame@1`; no RPC `inspect`; frame traces are per cell handler, so `rlm.frames` loses them after the cell (TS keeps them as session values). |
| A48 | verified | `acceptance_a48_end_to_end_repair_is_valid_exhaustion_is_incomplete_and_the_turn_continues` (+3 in `a47_a50_bounded_inference.rs`) | Host cases sent from real cells through the binary's session stack; end to end over RPC. The trace is read with `rlm.frames(id)` in the same cell instead of RPC `inspect`. |
| A49 | partial | 4 tests in `a47_a50_bounded_inference.rs` | No usage ledger (`activeReservations`, `modelCalls`); nested frames (`budget.depth > 1`) refused by the Rust host. |
| A50 | partial | `acceptance_a50_aborting_the_root_cancels_every_in_flight_frame_within_2s_ported_part`, `acceptance_a50_end_to_end_aborting_the_root_turn_cancels_every_running_frame_within_2s_ported_part` | Every frame connection drops within 2 s of RPC `abort`. No frame task journal (`cancelled` states), no RPC `inspect`, per-item map errors not observable after the abort. |
| A51 | partial | 3 tests in `a51_collapse_on_return.rs` | RPC test needs `agents.invoke`/`agents.result` of `rlm-child@1`; `ContextControl` is not wired into `session-core/src/compose.rs`. |
| A52 | partial | `acceptance_a52_ctx_history_summarize_guards_pin_note_and_state_follow_the_branch_ported_part` | `ContextControl` not wired into lanes; no extension event bus, note projection or `/rlm` panel; compaction ignores `kept_across_compaction`. |
| A53 | verified | `acceptance_a53_a_passing_skill_is_importable_listed_and_survives_worker_restart` (+5 in `a53_code_skills.rs`) | All six cases; the Pi skill list and runtime guide facts come from a real `ultron -p` system prompt in the same profile. |
| A54 | verified | `acceptance_a54_the_second_run_imports_the_saved_skill_and_makes_fewer_cheaper_calls` | Two RPC runs on one profile; needs the `lane.rs` hook above. |
| A55 | verified | 3 tests in `a55_agent_classes.rs`, 7 tests in `a55_agent_classes_registry.rs`, `acceptance_a55_a_class_defined_in_a_cell_is_invokable_typed_end_to_end_keeps_state_and_rejects_a_bad_return` | Every case of `ultron-agent-classes.test.ts` over a real kernel and the Rust typed-agent host, and the RPC end-to-end case of `ultron-a55-agent-classes.test.ts` through the binary: the session stack wires `Agents` (definition registry, journal, usage ledger, `rlm` lanes as durable child conversations) into every cell's requests. |
| A56 | partial | 13 tests in `a56_shell_jobs.rs`, `a56_async_events.rs` | Jobs journal persisted (`Host::with_journal`; the TS restart case ported with one deliberate difference: a job whose process still runs reads as running, not `interrupted`; SIGKILL evidence in `crates/host/tests/durable_jobs.rs`). Missing: `host::Host` reports no job ends (no `job_done`), no root ids/`cancelRoot`, jobs always lane "main"; no async-event dispatcher (coalescing, steer, budgets, chain cap); hints detection absent (only mute); `tools.*` extension tools not served. child_done summary omits the TS `definition: [verdict]` prefix. |
