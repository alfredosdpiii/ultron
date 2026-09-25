# Ultron A01-A46 acceptance instrument

This file is a reviewed checklist, not a claim that every row passes. Each row is marked `passed`, `blocked`, `failed`, or `unverified`. The worker cannot edit this file during an acceptance run.

| ID | Status | Evidence | Gap |
|---|---|---|---|
| A01 | passed | `ultron-native-host.test.ts`; direct and workflow task dispatch use `NativeRlmHost` | Combined model-facing parity remains unverified |
| A02 | unverified | Bounded RLM previews are tested | Large-data model-context fixture missing |
| A03 | passed | Definition repair and invalid-output tests | None for this runtime contract |
| A04 | passed | Explicit model forwarding tests and live `gpt-5.6-sol` smoke | Repeated capability trials missing |
| A05 | passed | Kernel and native-host cancellation tests | Full external process tree fixture missing |
| A06 | unverified | Kernel frame/output bounds are tested | Combined artifact/wall/concurrency/process limit matrix missing |
| A07 | passed | Session selection, fork, resume, continue, and task restart tests | Cross-service branch contamination suite missing |
| A08 | passed | Kernel snapshot and unsupported-state tests | Owner-bound snapshot provenance is not implemented |
| A09 | passed | Idempotency and usage reservation tests | Full nested tree-budget race matrix missing |
| A10 | unverified | Isolated test environment exists | Native worker credential/capability inspection is not an acceptance test |
| A11 | unverified | Optional controls are explicitly disabled | Approval scope/revision semantics are not implemented by default |
| A12 | passed | Workflow validation, dependency, cycle, and skip tests | Fan-in acceptance fixture missing |
| A13 | passed | Workflow admission waits for task result | Delayed provider integration fixture missing |
| A14 | passed | Failed/incomplete results remain `unverified` | Independent checker integration missing |
| A15 | unverified | Experiment records preserve attempts | Baseline/candidate checker is not release-gated |
| A16 | passed | Jev-gated memory tests cover skip and scope tags | Consolidation scope tests missing |
| A17 | passed | Memory correction and scope tests | Fresh-session contradictory claim fixture missing |
| A18 | passed | Memory failure/forget tests | Backend restart acceptance missing |
| A19 | passed | Refinement target protection tests | Capability/grant mutation policy is not enabled |
| A20 | unverified | Versioned refinement activation/rollback tests | Later behavior comparison is not wired to model execution |
| A21 | unverified | Durable terminal transitions are tested | Lost external-effect response fixture missing |
| A22 | unverified | Worker lifecycle tests exist | Detached retained-child reconnect acceptance missing |
| A23 | blocked | No native schedule/goal service is implemented | Requires an explicit scheduler design |
| A24 | passed | Usage ledger and task journal tests | Full event reconstruction report missing |
| A25 | unverified | Native and legacy session formats are separately handled | Full import/export/rollback rehearsal missing |
| A26 | unverified | Mutation and test-quality tooling exists | Independent acceptance instrument review not automated |
| A27 | passed | RLM kernel/tool tests with optional services absent | Model-facing self-contained run missing |
| A28 | passed | Memory scope and gate receipts are tested | Query rewrite abuse fixture missing |
| A29 | unverified | Memory list/why APIs exist | Authorized inspector command is not implemented |
| A30 | unverified | Forget/correction APIs are tested | Export/cache propagation acceptance missing |
| A31 | blocked | Skills remain on the compatibility path | Native skill selection is not implemented |
| A32 | passed | Refinement evidence/version/rejection/rollback tests | Jev refinement eligibility is not connected |
| A33 | passed | Experiment recording and matched-fixture checks | Variant-isolated model runs are missing |
| A34 | passed | Missing Jev/Hindsight fails closed | Combined optional-service outage run missing |
| A35 | passed | Deterministic, predict, and RLM task strategies share journal semantics | Full accounting parity fixture missing |
| A36 | unverified | Lane-specific kernels and durable task instances exist | Retained-instance API is not exposed natively |
| A37 | passed | Bounded output, result, error, cycle, and artifact range tests | Hostile custom repr model test missing |
| A38 | unverified | Reviewer tasks use dedicated lanes | Captured context exclusion fixture missing |
| A39 | unverified | Live provider prompt works | Typed model capability trial suite missing |
| A40 | unverified | Terminal task records remain immutable | Retained instance continuation is not exposed |
| A41 | blocked | Family messaging is not implemented in the native worker | Requires message service and identity policy |
| A42 | passed | Child inspection/cancellation tests | Human correction workflow is not exposed |
| A43 | unverified | Resource cleanup and task cancellation are tested | Evidence-aware eviction/quota policy is missing |
| A44 | blocked | No progress receipt or reassessment controller exists | Requires bounded progress service |
| A45 | passed | Nested kernel isolation and cancellation tests | Saturation capacity fixture missing |
| A46 | blocked | Core services and packed CLI are tested separately | Combined fix/review/retain/refine demonstration is not implemented |

## Compatibility routing

Every agent run, including RPC mode, uses the native RLM worker. These parts reuse Pi code inside the native path instead of rewriting it:

- `--help` and `--list-models` use Pi's handlers because they never run the agent.
- HTML export uses Pi's renderer on converted native entries.
- Extensions, skills, and prompt templates are loaded by Pi's resource loader inside the session worker. Extension commands run through a headless adapter.
- Themes use Pi's theme loader in the native TUI.

A reused Pi component is not counted as a native A01-A46 pass. It needs its own compatibility test or a later native port when a row requires one.
