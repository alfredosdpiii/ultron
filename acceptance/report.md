# Ultron A01-A46 acceptance report

Generated 2026-09-25T04:53:04.930Z by `npm run test:acceptance`. Unavailable is never passed; partial evidence is never a pass.

- Instrument lock: instrument lock matches
- Test runner: exit 0
- Rows: 5 passed, 0 failed, 39 unverified, 2 blocked (of 46)

| Row | Status | Evidence (passed/listed) | Reasons |
|---|---|---|---|
| A01 | unverified | 2/2 | evidence green but incomplete for this row. Background and agent dispatch share the durable task host. No parity suite compares direct, workflow-graph, and RLM invocation of the same definition and result contract. |
| A02 | unverified | 0/0 | no evidence. No evidence: no large-data fixture capturing request sizes against expected output. |
| A03 | unverified | 3/3 | evidence green but incomplete for this row. Typed failure on invalid output is tested. Bounded repair (maxRepairs > 0) with a semantically invalid result is not exercised. |
| A04 | unverified | 1/1 | evidence green but incomplete for this row. Child model preservation is tested for skill invocation only. Root pin capture and the live provider smoke require a metered live run (not part of this deterministic report). |
| A05 | unverified | 6/6 | evidence green but incomplete for this row. Cancellation and late-success races are covered for lanes and the kernel. A real external process-tree fixture is missing. |
| A06 | unverified | 3/3 | evidence green but incomplete for this row. Input and wall-time bounds only. No flooding/hanging worker matrix for output, artifact, concurrency, and process limits. |
| A07 | unverified | 4/4 | evidence green but incomplete for this row. Session selection and task recovery only. No branch/fork/resume state-restoration or abandoned-lesson leak test across restart. |
| A08 | unverified | 0/0 | no evidence. No evidence: no snapshot type matrix or tamper rejection test for Python kernel state. |
| A09 | unverified | 3/3 | evidence green but incomplete for this row. Idempotent concurrent invokes and one budget-admission failure path. No nested tree-budget race matrix, missing-usage, or unknown-pricing fixtures. |
| A10 | unverified | 3/3 | evidence green but mutation not killed: kernel-keeps-worker-control-channel (survived). Trusted-local profile only: environment inspection inside the real kernel and capability denial through host requests. Not a hostile-code isolation claim. |
| A11 | blocked | 0/0 | No evidence: approval scope/revision/policy binding is not implemented in the default native path. |
| A12 | unverified | 1/1 | evidence green but incomplete for this row. Validation-before-effects and cycle rejection are tested. Routes, fan-in, and explicit skip records lack a contract suite. |
| A13 | unverified | 0/0 | no evidence. No evidence: no slow-child fixture with a delayed valid result proving admission does not satisfy a dependency. |
| A14 | unverified | 4/4 | evidence green but incomplete for this row. No-check and failed-check paths stay unverified. Stale receipt and incomplete-review fixtures are missing. |
| A15 | unverified | 0/0 | no evidence. No evidence: no baseline/candidate checker with protected inputs. |
| A16 | unverified | 2/2 | evidence green but incomplete for this row. Skip-means-no-retrieval and scope tags are tested against a fake backend. Scope survival through consolidation and a real backend contract are missing. |
| A17 | unverified | 0/0 | no evidence. No evidence: memory correct() API exists but no contradictory-claim scenario across fresh sessions. |
| A18 | unverified | 2/2 | evidence green but incomplete for this row. Acceptance versus completed retention is tested. Forgetting persistence across backend restart is missing. |
| A19 | unverified | 1/1 | evidence green but incomplete for this row. Protected-target rejection only. Capability-escalation and policy/grant edit proposals are not exercised. |
| A20 | unverified | 2/2 | evidence green but incomplete for this row. Versioning and rollback bookkeeping are tested. No before/after/rollback task proving changed later behavior. |
| A21 | unverified | 2/2 | evidence green but incomplete for this row. Uncertain mutations stay explicit and schedule slots are not replayed. No lost-response external-effect service fixture. |
| A22 | unverified | 0/0 | no evidence. No evidence: no real supervisor/client disconnect and reconnect test with retained children. |
| A23 | passed | 10/10 | - |
| A24 | unverified | 0/0 | no evidence. No evidence: no event reconstruction or accounting reconciliation test tying costs, transitions, and inspection to one record. |
| A25 | unverified | 2/2 | evidence green but incomplete for this row. RPC compatibility only. No profile parity, import/export, or backup restoration rehearsal. |
| A26 | unverified | 0/0 | no evidence. mutation slice 9/10 killed; not killed: kernel-keeps-worker-control-channel. Evidence is this instrument: acceptance/instrument.lock (checked by the report) and the mutation slice in acceptance/mutation.json. Independent human review of the instrument is a process step, not automated, so the row cannot pass automatically. |
| A27 | unverified | 2/2 | evidence green but incomplete for this row. Kernel runs without optional services. No test asserts a self-contained task completes with zero graph, delegation, memory, or refinement dispatches. |
| A28 | unverified | 2/2 | evidence green but incomplete for this row. Scope enforcement on recall. No query rewrite failure or rewrite-widening test. |
| A29 | passed | 2/2 | - |
| A30 | unverified | 0/0 | no evidence. No evidence: no export/cache propagation or AGENTS.md preservation test for correction/deletion. |
| A31 | passed | 10/10 | - |
| A32 | unverified | 2/2 | evidence green but incomplete for this row. Evidence, validation, and rollback for refinements. Applicable approval is disabled and repeated-procedure/duplicate skill proposal cases are missing. |
| A33 | unverified | 1/1 | evidence green but incomplete for this row. Matched-fixture recording only. No memory/skill isolation between variants or failed-run usage reconciliation. |
| A34 | unverified | 3/3 | evidence green but incomplete for this row. Jev/memory fail closed. No combined outage run showing RLM continues without bypassing required checks. |
| A35 | unverified | 2/2 | evidence green but incomplete for this row. Deterministic path and adapter requirements. No strategy parity test for accounting or that predict launches no kernel. |
| A36 | unverified | 0/0 | no evidence. No evidence: retained-instance state versus invocation scratch is not exposed or tested. |
| A37 | unverified | 1/1 | evidence green but incomplete for this row. Artifact range reads only. No large-value, cyclic, or hostile-repr preview test in this suite. |
| A38 | unverified | 0/0 | no evidence. No evidence: no captured reviewer request or scope-restricted event query test. |
| A39 | unverified | 0/0 | no evidence. Requires the metered live capability suite (npm run eval:capabilities) with repeated trials; not run by this deterministic report. |
| A40 | unverified | 2/2 | evidence green but incomplete for this row. Terminal records are immutable. Retained-instance follow-up invocations are not exposed. |
| A41 | passed | 13/13 | - |
| A42 | unverified | 1/1 | evidence green but incomplete for this row. Single-child cancellation only. No two-child inspect/correct/stop run proving unrelated work continues. |
| A43 | unverified | 0/0 | no evidence. No evidence: no pinned checkpoint, live artifact, idle eviction, or quota test. |
| A44 | passed | 7/7 | - |
| A45 | unverified | 0/0 | no evidence. No evidence: no parent-awaiting-child saturation or bounded capacity failure test. |
| A46 | blocked | 0/0 | Combined fix/review/retain/refine demonstration is not implemented; also needs a live RLM run plus deterministic failure variants. |
