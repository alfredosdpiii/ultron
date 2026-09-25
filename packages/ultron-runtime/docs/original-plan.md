# RLM-first unified Pi implementation plan

Revision: 4. User-authorized in-place Pi build; all optional controls off by default. Consolidated implementation baseline incorporating the full discussion: RLM-first execution; NOOA-inspired typed methods, strategies, state lifetimes, and capability tests; Waku-inspired workflows and memory controls; Prime-inspired retained computation and messaging; Stencil authority boundaries; Gandalf evidence controls; Jev and Hindsight; controlled continual adaptation.

This document supersedes revisions 1 and 2. It is the single implementation plan, not a collection of optional competing architectures.

Status: implementation is directly in `~/.pi`, including the existing `agent/extensions/` files. No separate project, named application, or launcher. Back up replaced files and test changes before activating them; do not reload the active session mid-build.

## Controlling amendment: no permission friction

The user's latest instruction supersedes conflicting defaults below. Pi's new services default to trusted-local execution with permission prompts, risk-screen blocking, capability allowlists, budget enforcement, forced completion gates, mandatory refinement approval, and forced sandboxing OFF. No repeated confirmations for tools, delegation, memory, or local refinements. Controls are individually opt-in; no unavailable judge creates a permission prompt in the default profile.

Usage recording, strict input/output validation, honest verification status, explicit cancellation, protocol/output bounds, and corruption detection remain correctness mechanisms, not permission gates. Default resource policy does not impose the proposed canary budget/task caps; explicit call deadlines and bounded presentation/protocol frames still work. Not running checks yields `unverified`, not a fabricated pass. Opting out of controls never lets learned content rewrite human-owned policy or enables unrequested external actions.

A stricter profile may enable specific controls explicitly. The acceptance matrix must exercise BOTH no-prompts/default-off behavior and opt-in enforcement. Any later statement requiring grants, forced gates, approval, or isolation applies only to an enabled control or explicitly constrained task. Profile `isolated` must never claim containment when unavailable; default profile makes no sandbox claim. Automatic publication/deployment remains outside scope.

Implementation progress: see `~/.pi/docs/implementation-status.md` for the connected typed-agent/Python/workflow slice, observed tests, and remaining gaps. The full acceptance matrix below is not yet satisfied.

Baseline inspected: 2026-09-23 Asia/Manila. Pi 0.84.4; separate Prime Agent 0.9.1; Gandalf checkout b9b96eec77ca59532d620c58ed84ba99ccf2b425.

## 1. Outcome

Build one Pi-based system in which RLM is the primary execution model, specialists are typed reusable agent definitions, optional graphs compose those same agents, Jev supplies semantic decisions, Hindsight stores durable observations, and evidence drives controlled continual adaptation.

RLM is the default reasoning and execution path. The root may answer directly, compute locally, invoke specialists, or call a known workflow. Neither a graph nor Jev selects a mandatory sequence of reasoning steps for every prompt. Memory, skills, and graphs are services the RLM can use; host policy supplies non-optional execution and completion boundaries.

Success means these components work together across ordinary execution, cancellation, compaction, branch changes, restart, and refinement. Independently working helpers are not sufficient. A small self-contained question must remain a small interaction, without compulsory delegation, workflow execution, memory retrieval, or refinement.

The defining contract:

> Direct invocation, a graph node, and recursive delegation invoke the same versioned agent definition through the same task service. They share validation, permissions, budgets, artifacts, events, and completion evidence.

### Completion in one sentence

The system is complete when an RLM task can process large inputs, invoke a typed specialist directly or through a reusable graph, inspect and retain that specialist, survive supported interruptions, produce independently checked artifacts, learn a scoped reusable improvement, and demonstrate that improvement on a later task without bypassing policy or losing provenance.

### Reading guide

- Sections 2-5: architecture, ownership, and public contracts.
- Sections 6-10: execution, security, memory, workflows, and adaptation.
- Sections 11-12: code layout, delivery phases, and the first implementation slices.
- Sections 13-14: acceptance cases and commands that establish completion.
- Sections 15-17: migration, scope limits, and sources.

All APIs, commands, limits, and tests described as proposed are implementation requirements, not claims about current Pi.

## 2. Design decisions

1. **RLM-first, not graph-first.** The root agent decides how to investigate and decompose work. No mandatory classification graph before every prompt. Known procedures are optional callable workflows. Required safety and completion checks are host policy, not optional workflows.
2. **Pi remains Pi.** Preserve the interactive experience, providers, history, and extension compatibility. Do not point `pi` at Prime or relabel Prime as Pi. Keep `prime-agent` independently available.
3. **One trusted host owns execution policy.** Python is a computation environment, not the authority for permissions, spend, task state, or completion.
4. **One lifecycle implementation.** Shell jobs, Python executions, agent invocations, workflow nodes, and scheduled continuations use shared job mechanisms rather than separate registries.
5. **Typed definitions, ordinary Python composition.** Agent state, input/output contracts, capabilities, and instructions belong to one definition. No separate swarm DSL.
6. **Jev advises; deterministic code enforces.** Classification cannot authorize new capabilities, declare a test passed, override a budget, or manufacture evidence.
7. **Hindsight stays the memory backend.** Inspect its actual APIs and reconciliation behavior before adding consolidation. Do not import Waku SQLite or Gandalf pgvector alongside it.
8. **Adapt ideas and selected code, not whole applications.** Do not run Waku, NOOA, Gandalf, and Prime as four competing orchestrators. Preserve attribution and licenses for any adapted code.
9. **Retain direct read/edit/write/bash initially.** Route direct tools and Python-host calls through the same services. A single Python tool is not itself a success criterion.
10. **No silent promotion.** New execution, sandbox, scheduling, or automatic refinement behavior is staged behind an explicit profile and reversible installation.

### Source responsibilities

| Source | Adopt | Do not import by default |
|---|---|---|
| Prime / RLM | Persistent computation, context as data, recursive invocation, retained agents, resource release, family messaging, intervention, verified progress, continuation semantics | A second provider/session authority or a Prime launcher shim |
| NOOA | Typed agent methods/state, predict versus iterative strategies, scoped scratch, live values, bounded previews, context/events, model-facing capability tests | In-process live references across isolated workers or a second provider loop |
| Waku | Optional graph structure; explainable memory gates; inspectable/correctable memory; procedural-memory proposals; selective skill loading; consolidation; eval and comparison history | Mandatory triage before the root, gateways, calendar tools, another memory database |
| Stencil | Single authority, bounded jobs, branch correctness, views as projections | A Rust rewrite, XML DOM, or complete renderer rewrite as prerequisites |
| Gandalf | Evidence receipts, scoped approvals, budget admission, fencing, effect reconciliation | Slack/FastAPI application, organization-specific delivery rules, Redis/Postgres merely for reuse |
| Jev | Specialist/model routing, memory decisions, risk and refinement judgments | Enforcement authority or a second agent loop |

## 3. Current state and migration obligations

Current implementation paths:

- `~/.pi/agent/extensions/prime-rlm/{index.ts,kernel.ts,registry.ts,runtime.py}`: local Python runtime and Pi RPC children, not Prime's native runtime.
- `~/.pi/agent/extensions/jev/`: routing, memory, compaction, refinement, judgments, and optional screening.
- `workflow.ts`: dependency-wave helper, currently used only by `memory-gate -> memory-recall`.
- `~/attic/gandalf/pi-extensions/self-review/`: working-tree review package, currently untracked in that checkout.
- `~/.pi/agent/settings.json`: Cursor provider, self-review, MCP adapter.
- `~/.pi/agent/mcp.json`: Exa Agent stdio server with restricted environment inheritance.

Known gaps:

- No common agent-definition/result contract.
- Python snapshots are session-file scoped, not branch-correct.
- Persisted child records do not provide retained-child recovery.
- No shared recursive usage ledger or preflight budget service.
- Python output is not centrally bounded; running cells lack enforced cancellation/deadlines.
- Refinement appends session lessons, without typed CRUD, activation versions, or rollback.
- Jev refinement restoration scans all session entries, not just the selected branch.
- Approval bypass defaults on; current-session traces confirmed bypass. This is not an active enforcement guarantee.
- No unified release gate for the combined system.

Preserve credentials, existing session files, trust decisions, provider configuration, skills, Cursor support, Exa environment restrictions, judgment tools, and self-review behavior. Do not copy secrets into a source repository or evidence bundle.

Keep current model policy initially: root Astra/high; simple children GLM Flash; difficult/architecture children Astra; design children Kimi. Explicit model choice wins within allowed policy. No silent model substitution. Snapshot the resolved policy per task rather than trusting stale memories.

## 4. Target architecture and ownership

```text
Pi UI / CLI / RPC
        |
Trusted Pi host services
  Session authority + admission + policy + budgets + job lifecycle
        |
Root RLM execution
  persistent Python / data handles / ordinary control flow
        |
        +-- agents.invoke(definition, input)
        +-- rlm.spawn(definition, input)
        +-- workflows.run(definition, input)
        |       +-- invokes the SAME task service
        +-- jev.triage / memory / context / events
        |
Validated results + artifacts + check receipts
        |
Evidence -> refinement proposal -> verification -> versioned activation
```

### Default task flow

1. Pi authenticates the request, resolves the execution profile, establishes task identity/budget, and prepares bounded input handles.
2. The root RLM receives the task. Relevant memory may be prepared under the configured retrieval policy, but there is no graph-dispatch prerequisite. A self-contained request may skip memory and all supporting model calls.
3. The root chooses direct reasoning, Python computation, typed specialists, or an optional workflow. It can discover procedures without activating them.
4. When delegation is requested, Jev recommends a permitted specialist/model if no explicit selection already applies. The host validates the selection and admits the child through the shared task service.
5. Results remain typed values/artifacts. The host requires the task's declared evidence before accepting a verified-completion claim.
6. Consolidation and skill/refinement proposals run as bounded maintenance jobs when eligible. They do not add a mandatory wait after every answer.

A scheduled task follows the same flow; a workflow does not become a second root agent. The later candidate-yield controller enforces continuation and completion policy, not a predetermined reasoning strategy.

### Authority rules

The host must own one recoverable logical record of transcript, task/job transitions, graph progress, context selections, definition versions, approvals, queues, and policy configuration. Runtime maps and UI caches are derived from that record.

Not everything rewinds:

- Branch-local planning state, context blocks, definition selections, and checkpoints follow the selected branch.
- Actual spend, external-effect history, revocations, and user-requested deletion are monotonic records. Rewinding never refunds spend or resurrects revoked authority.
- External filesystem/network effects are not undone by transcript rewind. Record them and require explicit restore/reconciliation.
- Shared memory is an external service, not a branch-rewindable database. Record the exact recalled observations and versions used by each task. A new branch may query current memory without pretending it is the historical result.

### Storage decision

Target a local single-host service with a transactional SQLite event/job/usage store and content-addressed artifact files. This is execution storage, not a replacement memory store. One serialized writer owns transitions; schema migrations and backup/export are mandatory.

The Pi integration must make transcript and host state part of one logical commit/recovery protocol. Stock Pi JSONL and a new database must not both independently determine task truth. Choose the concrete integration in phase 0:

1. Prefer Pi SDK runtime/service injection with a canonical event commit path and a Pi-compatible transcript export/projection.
2. If the public SDK cannot intercept persistence, candidate yields, branch transitions, and all billable calls correctly, use a small maintained Pi host patch/fork with explicit branding/version metadata.
3. Do not replace this requirement with best-effort dual writes. If compatibility mode temporarily retains stock JSONL authority, label that mode non-durable and prohibit background/recovery claims until the integration gate passes.

An extension-only solution is acceptable only if it passes the same tests. This plan does not assume that it can.

## 5. Shared public contracts

Use versioned JSON Schema as the cross-process source of truth. Generate or mechanically verify TypeScript and Python bindings in CI. Validate at both sides of every untrusted RPC boundary. Keep schemas strict, with explicit compatibility migrations.

| Contract | Required fields and behavior |
|---|---|
| `AgentDefinition` | ID/version/hash; instructions; typed method declarations with strategy and schemas; explicit state schema/lifetime; permitted capabilities; model-route policy; resource limits; context visibility and validation/repair policy |
| `AgentInstance` | Stable instance ID; definition hash; owning session/branch; state version; kernel/checkpoint references; active invocation; retained/closed status |
| `MethodDefinition` | Name/docstring; input/output schemas; deterministic/predict/rlm strategy; bounded repair; context selection; permitted capabilities; state access policy |
| `AgentMessage` | Message ID; host-derived sender/family; recipient instance/task; delivery mode; schema version; bounded text/artifact references; expiry; delivery state |
| `ProgressReceipt` | Task and milestone IDs; acceptance criterion; verifier/evidence references; candidate revision; time and cumulative usage; provisional versus verified status |
| `TaskRequest` | Definition hash; input or artifact references; session/branch/parent IDs; workspace revision; grant ID; budget account; idempotency key |
| `TaskHandle` | Stable task ID; confirms admission only; inspect/await/message/cancel operations |
| `TaskResult` | Tagged validated value, failure, cancellation, needs-input, or uncertain outcome; artifacts and evidence IDs; never a bare text-as-success convention |
| `JobRecord` | Kind, owner generation, lifecycle, deadlines, parentage, workspace ownership, execution profile |
| `ArtifactRef` | Content hash, type, size, owner/scope, provenance, bounded-preview metadata; no secret-bearing public URLs |
| `ContextBlock` | Name/version; static or dynamic; provenance; visibility; token budget; branch ownership |
| `DecisionRecord` | Decision kind and policy version; decision; confidence where available; allowed candidates; concise reason; evidence references; latency and usage |
| `RetrievalPlan` | Retrieve/skip/unavailable; reason; original task reference; effective query; requested and host-authorized scopes; result/token limits; query-rewriter version if used |
| `MemoryView` | Backend ID/version; source and observation time; scope; evidence class; supersession/deletion status; task-specific retrieval provenance; read-only projection |
| `SkillSelection` | Skill/definition ID and content version; selected by root/user/workflow; reason; metadata-only versus loaded; context cost; effective grants unchanged |
| `ExperimentRun` | Frozen task-set/configuration hashes; variant; attempts; revisions; model settings; memory/skill snapshots; outcomes; latency; usage; unresolved costs; evidence links |
| `CheckReceipt` | Candidate revision/tree hash; base revision; checker/config version; commands; outcomes/skips/timeouts; evidence hashes |
| `RefinementProposal` | Target kind/ID/base version; proposed change; evidence IDs; scope; reason; validation results; activation/rollback history |

### Python ergonomics

Proposed APIs, not current functionality:

```python
reviewer = agents.get("security-reviewer", version="1")
job = await rlm.spawn(reviewer, input={"change": change_ref}, name="security")
result = await job.result()  # Validates the definition's output schema.

report = await workflows.run("change-review", input={"change": change_ref})
recent = await events.query(task_id=job.id, kinds=["check.completed"], limit=10)
await memory.propose(observation, evidence=[receipt.id], scope="repository")
```

`agents.invoke()` is admission plus await through the same service, not another loop. Admission selects the method's execution strategy; a predict call need not start a Python kernel or a full coding-agent subprocess. Model-created ad-hoc definitions are task-local, schema-validated, and cannot widen inherited permissions or limits. Promoting one to a reusable definition follows the refinement process.

### Agent methods and execution strategies

One definition may contain several typed methods:

| Strategy | Execution | Use |
|---|---|---|
| `deterministic` | Registered reviewed code, no model inference | Parsing, exact rules, artifact checks |
| `predict` | One typed model response plus explicitly bounded validation repair | Classification, small extraction, semantic judgments |
| `rlm` | Persistent computation and an iterative model loop, with recursive delegation available | Investigation, coding, large-data processing, open-ended work |

The root uses the RLM strategy. Do not replace it with a classifier-plus-workflow dispatcher. Where a predict method matches Jev's supported judgments, use Jev through the shared provider/decision service; do not duplicate classifiers merely to fill an agent catalog.

All strategies use the same task IDs, grants, usage ledger, cancellation protocol, output validation, and evidence format. A deterministic method executing generated or repository code still needs a killable execution boundary. Strategy selection cannot bypass policy.

A Python authoring adapter may derive method names, docstrings, and schemas from classes into the canonical definition manifest. Declaration discovery must not execute untrusted module-level code on the host. Begin with reviewed manifests and generated typed Python proxies; add class-authoring sugar only after manifest parity tests pass. This preserves NOOA's programming model without making arbitrary Python objects host authority.

Repair validates shape and declared deterministic constraints. Schema-valid output is not proof of factual correctness; verification and evidence rules remain separate. After repair exhaustion return a typed validation failure, not the last malformed answer.

Live Python values remain by reference inside one kernel. Cross-worker inputs use validated serialization or immutable artifact references. Never pretend JSON RPC transfers live Python object identity.

## 6. RLM and context behavior

- Load large task inputs into scoped data/artifact handles before inference where the ingress mode permits it. Give the root bounded metadata and access instructions, not the entire data payload.
- Retain intermediate values in Python or artifacts. Print bounded previews; preserve full authorized content for range reads and computation.
- Provide a final-result operation accepting typed values or artifact references. Large outputs do not have to fit in one assistant response.
- Render stable instructions and definition metadata as a cacheable prefix, typed event history in the middle, and bounded dynamic state at the tail.
- Expose read/query and permitted context-selection APIs to the model. Source events are immutable; collapse changes the model-visible projection, not the evidence.
- Preserve original history behind compaction. Associate each compaction with a branch/sequence snapshot; discard stale speculative work. First make synchronous compaction correct; add speculative scheduling only after correctness tests pass.
- Use checkpoint-safe values plus an explicit manifest of non-restorable values. Do not promise arbitrary Python replay. Never automatically rerun arbitrary cells with possible effects.
- Treat dill/pickle snapshots as executable trusted artifacts. Do not load user/repository-supplied snapshots. Prefer constrained formats; bind snapshots to owner, runtime, definition, and branch versions.

### State lifetimes and bounded inspection

| State | Owner and lifetime | Restore behavior |
|---|---|---|
| Invocation scratch | One method invocation; persists across its Python cells | Discard after invocation; recover only from an explicit matching checkpoint |
| Retained agent state | One `AgentInstance`; serialized methods update versioned state | Restore its validated state/checkpoint; do not leak another instance's locals |
| Root working computation | Root RLM kernel within its branch | Keep across root turns; branch change requires matching checkpoint or explicit reset |
| Session control state | Trusted host journal | Reconstruct from committed events |
| Reusable knowledge | Scoped Hindsight observations and versioned definitions | Retrieve/select explicitly, respecting current grants and deletion policy |

A completed invocation and a retained agent instance are different objects. Continuing a retained agent creates a new invocation/task ID under the same instance; it never changes the old task's terminal result. Retention is opt-in or policy-bounded, not keeping every child alive forever.

Provide `describe(definition_or_handle)` and `preview(value, depth=..., max_bytes=...)`. Preview shows type, actual length where safely obtainable, selected head/tail values, and truncation metadata. Full values remain usable through computation or range reads. Cross-process handles reveal authorized metadata only; arbitrary object `repr` and property access run inside the worker with limits, not on the host. Invocation output and model-visible preview have distinct contracts.

Nested calls receive an explicit context selection. A reviewer receives requirements, immutable diff, and selected receipts, not automatically the implementer's speculative conclusions or unrelated conversation. Record the exact event/artifact references supplied. The model can request additional permitted context; it cannot read hidden evaluation cases or expand its own grant.

Serialize external invocations on a stateful agent instance. Define same-instance nested calls as a suspended caller stack, rather than acquiring the same non-reentrant lock twice. Distinct instances can run concurrently. No shared mutable state across isolated workers without an explicit host transaction.

## 7. Jobs, limits, and execution security

Invocation lifecycle: admitted -> queued -> running -> terminal, with waiting-for-input an explicit nonterminal state. Terminal outcomes distinguish success, failure, cancellation, timeout, budget exhaustion, and uncertain effects. Retained-idle belongs to the agent-instance lifecycle, not a completed task. Completion evidence is separate from execution state.

- Persist admission before launch. A restart between admission and launch must not duplicate effects.
- Use owner generations/fencing for every mutation. After cancellation, late results remain diagnostic but cannot commit task success.
- Enforce root-tree concurrency, depth, token/request budgets, wall time, output bytes, artifact quota, and supported process-tree resource limits.
- Reserve spend atomically before dispatch; settle against provider usage. Every retry, specialist, Jev call, review, and refinement is accounted for. Missing usage stays unresolved, not zero.
- Separate provider-reported usage, estimated currency, external tool charges, and unmetered services. Unknown pricing means no verified hard-dollar guarantee; enforce token/request caps or reject a hard-dollar task.
- Child workspaces are read-only or isolated writable copies/worktrees. Concurrent writers never share one mutable checkout by default. Applying a child diff is a host-authorized operation with base revision checks.
- Cancellation escalates cooperative interrupt -> grace period -> process-group/container termination. No unbounded output collection before truncation.
- External writes use prepare/authorize/execute/reconcile records. Outboxes provide durable delivery attempts, not universal exactly-once effects.

### Initial canary limit profile

These are proposed configurable starting values, not measured production defaults. Freeze the actual profile in P0 and record it with every acceptance run. Increase limits explicitly when a fixture requires it; never hide budget resets inside repairs or nested tasks.

| Limit | Proposed canary value |
|---|---|
| Recursive depth | 3 descendant levels, preserving current intent |
| Active child model requests | 3 per root tree; all descendants share the cap |
| Admitted unfinished invocations | 24 per root tree |
| Typed-result repair | At most 2 extra attempts, charged to original task |
| Python cell deadline | 60 seconds; long work must be admitted as a job |
| Foreground wait | 10 seconds, then expose a pending handle without implying cancellation |
| Child task deadline | 15 minutes, capped by remaining parent deadline |
| Root unattended deadline | 30 minutes, explicit continuation needed to extend |
| Root request/token envelope | 100 provider attempts and 200,000 aggregate input/output tokens; cache-token accounting specified by provider adapter |
| Model-visible preview | 8 KiB per value/result; context assembler also enforces total token budget |
| RPC frame / message | 1 MiB / 16 KiB; larger payloads use artifacts |
| Artifact storage | 256 MiB per task and 1 GiB per root tree |
| Retained idle agents | 4 per root, 30-minute idle TTL unless explicitly pinned within quota |
| Message queue | 32 pending messages per recipient; 30-minute expiry |
| Cancellation grace | 2 seconds before enforced termination |

Avoid nested scheduler deadlock: a parent awaiting children must not occupy a child-model execution slot needed by those children. Keep separate limits for live kernels, runnable model requests, and admitted tasks. Reject excess admission with a typed capacity result; do not build an unbounded hidden queue.

Execution profiles:

| Profile | Contract |
|---|---|
| `trusted-local` | User-permission execution, prominently labeled; screening is advisory defense in depth, not containment |
| `isolated` | Container/VM or equivalent; restricted mounts, process-tree limits, controlled network, no provider/host secrets; deny if required isolation is unavailable |

Keep model/provider credentials and publication credentials on the trusted host. A worker receives a narrow job grant. Bound and authenticate host RPC; a job token alone is not OS containment. MCP and direct tools must respect the chosen profile as well as Python execution.

Existing auto-run behavior remains unchanged during planning/migration. The proposed managed profile uses explicit scoped grants; any relaxed mode requires deliberate selection and a visible warning. Do not silently turn on approvals or change user defaults during implementation.

### Retention and resource release

Expose `retain`, `checkpoint`, `archive`, `release`, and `close` through the shared instance/artifact services. The root chooses which computation is useful; the host enforces quotas, ownership, expiry, and deletion policy.

- Retain a specialist for follow-up without re-creating its identity.
- Archive validated results and evidence before closing an idle kernel.
- Release large intermediate values explicitly; live Python locals require worker cooperation or whole-kernel termination to reclaim.
- Refuse deletion of an artifact still required by a live task/checkpoint, or create an explicit invalidation transition. Never leave a silently dangling handle.
- Automatic eviction targets idle, unpinned resources and records what was evicted and whether it is restorable.
- Artifact GC follows host reference/retention rules. Agent requests cannot erase spend, effect history, or required evidence under the guise of cleanup.

### Family messaging and human intervention

Use host-routed typed envelopes, not unrestricted peer networking. The host derives sender identity and verifies family membership, scope, quotas, expiry, and permitted artifact access. A message carries information, not new grants. Record queued/delivered/consumed/expired/rejected separately; delivery is not task completion. Deduplicate message processing with message IDs, while still reconciling any uncertain external effect.

Default delivery is follow-up at a safe invocation boundary. Explicit steering is queued until a supported checkpoint; it cannot asynchronously mutate executing Python. Define FIFO within each sender/recipient stream; make no global ordering claim across senders. Retained recipients create a new invocation when processing accepted follow-up work. Sibling messaging is permitted only for approved family relationships; required independent verifiers cannot receive contaminating implementer messages outside their evidence policy.

Expose proposed Pi controls over the same services:

| Command | Behavior |
|---|---|
| `/agents` | Inspect root/child instances, active tasks, models, definition versions, remaining budget, and evidence |
| `/agents inspect <id>` | Bounded authorized transcript/state preview; no fresh model call |
| `/agents send <id>` | Targeted human correction with explicit steer/follow-up semantics |
| `/agents stop <id>` | Cancel that subtree without stopping unrelated work |
| `/agents retain <id>` | Request bounded retention under quota |
| `/agents close <id>` | Close retained computation while preserving required results/history |

Keep `/rlm` and `/rlm-children` as aliases/projections where compatible. UI detachment is not cancellation; explicit stop is. Cancellation propagates down the selected task subtree; siblings survive unless the workflow's recorded failure policy requires cancelling them.

## 8. Jev, Hindsight, and inspectable memory

Jev services: specialist/model routing, retrieval gate, retention classification, refinement eligibility, and optional risk judgment. Preserve existing test-quality and mutation-evidence tools as advisory review services.

### Explainable retrieval without mandatory orchestration

Implement `memory.prepare(task, requested_scope)` as a shared service usable by root context preparation or an explicit RLM call. Both paths reuse a compatible task/query/scope result instead of independently retrieving twice. Explicit refresh and changed input produce new recorded decisions; scope checks are never bypassed by a cache hit.

Record a `RetrievalPlan` containing retrieve/skip/unavailable, a concise reason, effective query, authorized scopes, and limits. A skip performs no backend retrieval. An explanation is decision metadata, not hidden chain-of-thought.

Jev supplies constrained relevance and scope recommendations. First verify which output forms its installed API supports. If semantic query rewriting requires a generative call, use a separately metered bounded step; record its model/version and preserve the original request reference. The rewriter cannot widen scope. Failure uses the original query inside the authorized scope when policy permits, or reports memory unavailable.

Support `off`, `on-demand`, and `gated-auto` retrieval policies. P0 selects the canary policy explicitly and records it in experiments. None forces a graph or specialist invocation; ordinary self-contained tasks must have a tested no-memory path.

Failure policies:

- Optional routing failure: use an explicitly configured safe definition/model or report unavailable; never silently switch the pinned root.
- Memory unavailable: continue without memory when the task permits it; record the limitation.
- Refinement judge unavailable: do not auto-activate a proposal.
- Required schema/check/grant failure: block the corresponding transition.
- Safety classification unavailable: apply deterministic grant policy; do not infer approval.

Memory records distinguish user statements, observed tool evidence, assistant hypotheses, and verified conclusions. Preserve source time separately from ingestion time. Current configuration comes from live config/evidence, not remembered assistant claims.

Implement scoped propose/correct/forget/recall operations. Enforce scope before retrieval and consolidation. Corrections link to superseded evidence rather than rewriting history. Hypotheses do not become current facts just because Jev voted keep.

Inspect Hindsight's installed version for ingestion status, deduplication, updates, deletion, consolidation, and scope filters. If required behavior is unavailable, add a narrow host-owned provenance/scope adapter or mark the operation unsupported. Do not claim a memory is stored because an asynchronous request was merely accepted.

### Facts, episodes, and procedures

- Facts and preferences are scoped observations in Hindsight, with evidence class and supersession links.
- Episodes refer to a specific task and its outcomes. Large execution evidence remains in the artifact/event store; memory holds an authorized summary and references.
- Procedures are versioned skill/workflow definitions, not ordinary memory prose. Converting an episode into a procedure follows section 10.

Consolidation is a bounded background job with retained source evidence and retry state. Schedule after a configurable number of eligible interactions or at idle time, deduplicate maintenance admission, and retain unconsolidated source references after failure. Inspect Hindsight's own consolidation before adding another summarizer. Do not merge across scope/identity boundaries or discard a source document's chunks as duplicate facts.

User-requested forgetting follows authorization and deletion policy, not a usefulness vote. Redact durable telemetry and prevent logs/checkpoints from reintroducing deleted secrets. State retention limits explicitly. Keep non-content deletion markers and audit metadata where permitted, not forgotten content disguised as immutable evidence.

### Memory inspection and correction

Provide thin Pi commands over the same memory service available to Python:

| Proposed command | Behavior |
|---|---|
| `/memory` | List scoped memories relevant to the current task, with bounded previews |
| `/memory why [task-id]` | Show the recorded gate reason, effective query, scope, and which entries actually entered context |
| `/memory show <id>` | Show authorized content, provenance, evidence class, source/observation times, and supersession state |
| `/memory correct <id>` | Capture a correction with source and evidence, then show proposed/applied/pending status accurately |
| `/memory forget <id>` | Request authorized deletion and report backend completion versus pending/failure |

Inspection never starts a fresh retrieval just to invent an explanation for an old task. A user correction is recorded as a user statement, not automatically as independently verified tool evidence. Confirmation is required for destructive or shared-scope changes according to policy.

Hindsight owns memory content. The host owns authorization, operation tracking, and task-specific decision records. TUI lists and any opt-in Markdown export are read-only projections, never an editable second memory authority. Exports respect scope and redaction, and deletion invalidates managed caches/exports. No writable SOUL.md or AI edits to trusted AGENTS.md.

Do not put every memory operation in the permanent tool roster. Expose it through the stable Python API and these user commands.

## 9. Optional graphs and the first workflow

Keep a compact workflow engine around the ordinary task service. Add conditional edges, explicit skipped states, fan-out/fan-in, concurrency limits, bounded revision cycles, typed outputs, and topology-derived inspection.

Validate the complete topology and output bindings before any node runs. Nodes read immutable prior results and publish namespaced outputs; shared merges require declared reducers. On failure, apply an explicit cancel/collect/continue policy and preserve completed sibling evidence.

Fallback to plain RLM is allowed only for optional dispatch before consequential work, or after safe reconciliation. Never rerun a partially effectful graph blindly. Required verification cannot be bypassed by fallback.

First workflow: `change-review@1`.

```text
Capture immutable diff + base/candidate revision + requirements
    |
    +-- correctness-reviewer --+
    +-- security-reviewer -----+--> finding-verifier --> report
    +-- test-reviewer ---------+
    +-- host check runner -----+
```

Adapt the current Gandalf-derived self-review prompts, bounded diff selection, and honest verdicts into these definitions. Port `/self-review` to this workflow without losing report persistence or follow-up review. The host check runner emits receipts, not model opinions. A separate tests-writer may add tests in an isolated writable workspace; reviewers stay read-only.

A later `implement-and-verify` workflow adds a bounded repair loop. It does not authorize publication or deployment by default.

### Selective procedure discovery and loading

Keep a small metadata catalog of permitted skills, agents, and workflows. The root can search that catalog and inspect a definition before invoking it. Reuse Pi's existing skill discovery through an adapter; do not add a competing loader.

Record `SkillSelection` events for every activation: exact version/hash, who selected it, why it was selected, and its context cost. Discovery is not activation, and activation grants no capabilities. Effective permissions remain the intersection of host grant and definition policy.

Show selections through `/skills why [task-id]` and the task inspector. Pin definitions during an invocation so a concurrent refinement cannot change its instructions mid-run. Context assembly loads only selected instructions under its budget; selection does not require changing the permanent tool schema roster.

The default RLM path must remain usable with all optional workflow definitions disabled.

## 10. Continual adaptation

Four target kinds: observation, supplemental instruction, executable skill, and agent definition. Workflow-definition changes may follow after those paths are proven.

1. Record a failure or successful reusable procedure with evidence.
2. Jev decides whether refinement is worth attempting.
3. A refiner proposes a minimal versioned edit against a specific base version.
4. Validate schema, scope, capability non-escalation, and supporting evidence.
5. Run component tests and a held-out behavioral check where applicable.
6. Apply atomically at an idle/turn boundary; record activation and intended effect.
7. Compare later outcomes and allow explicit rollback.

Session-local non-executable observations may be auto-accepted under policy. Global promotion, executable changes, and capability changes require explicit review. Trusted AGENTS.md, security policy, judge thresholds, and the acceptance instrument are not writable refinement targets.

Do not equate an applied lesson with an improvement. Measure later behavior. Contaminated refinements remain traceable and can be deactivated. The Prime paper's example of retaining an objective exploit as a skill is a required negative test case.

### Deliberate skill-promotion proposals

Add a user-facing proposal path for recurring successful procedures:

```text
Execution receipts and repeated outcomes
    -> Jev usefulness decision
    -> minimal skill or workflow proposal
    -> schema/capability checks and tests
    -> user review where required
    -> versioned activation
    -> measured later use / rollback
```

A proposal names the problem it solves, supporting runs, required capabilities, expected input/output, tests, scope, and changes from an existing definition. Search for an existing equivalent before proposing a duplicate. Do not install a procedure simply because the model calls it reusable.

Provide `/refine proposals`, `/refine show <id>`, `/refine approve <id>`, `/refine reject <id>`, and `/refine rollback <activation-id>` as views/actions over the same proposal service. Preserve existing manual/status command compatibility. Approval does not override failed validation or silently grant new capabilities.

Offer proposals at task boundaries with a configurable frequency cap; declined proposals should not be repeatedly resurfaced without new evidence. Skill discovery may expose a newly activated version to later tasks, never mutate an in-flight task.

### Refining coordination rather than adding agents by habit

Repeated outcomes may justify changing the division of labor: a specialist role, method strategy, context selection, or workflow stage. Proposals must identify a failure pattern or measured benefit and compare against the simpler current procedure. More agents and more steps are not success metrics.

Agent-definition updates are available in P4b. Workflow-definition mutation remains disabled until bounded-cycle, policy, invocation-parity, and comparative evaluation cases pass. A new coordination pattern uses the existing task service and cannot add a second scheduler or rewrite the root's execution model.

### Verified progress on long-running work

Record `ProgressReceipt` milestones with acceptance criteria, evidence, revision, time, and cumulative spend. Separate model-reported progress from independently checked milestones. A passing check verifies only its declared behavior; an activity counter does not demonstrate useful progress.

For unattended work, evaluate progress at a fixed configured interval and at budget thresholds. Lack of new verified milestones may trigger a metered Jev reassessment recommendation or human handoff; it is not proof that the task is impossible or permission to increase budgets. The host owns limits, while RLM chooses the next strategy within them. Tasks without an objective verifier explicitly report progress as unverified.

### Experiment history

Add a persistent comparison ledger, not a new evaluation engine. Each run references frozen task/configuration hashes, exact definition versions, models/thinking levels, memory state, tool grants, retries, receipts, and full-tree usage. Record incomplete and failed attempts as well as successful ones.

Expose `/experiments` and `/experiments compare <baseline> <candidate>` with a compact report:

| Field | Reporting rule |
|---|---|
| Outcome | Deterministic acceptance separately from judged quality |
| Latency | End-to-end duration and supporting-service time |
| Cost | Root, descendants, Jev, review, refinement, external charges, and unresolved liability |
| Memory | Gate/query version, injected entries, and scope |
| Procedures | Selected skill/agent/workflow versions and repairs |
| Uncertainty | Sample counts, repeated runs, missing evidence, and limitations |

Use task-specific isolated memory banks/snapshots where supported, or captured read-only retrieval fixtures. Never let one experimental variant train the other's memory/skills. Deliberately longitudinal refinement experiments record their state progression instead of pretending each run is independent.

Compare RLM-only against RLM plus optional memory, graph, or refinement one change at a time. No variant may disable required safety checks to appear faster. Experiment history and usage survive UI resets, but their sensitive content follows retention/deletion policy. Markdown/JSON reports and TUI tables derive from the same ledger.

## 11. Implementation location and migration layout

Source root: `~/.pi/`, as explicitly requested by the user. Edit existing extensions in `~/.pi/agent/extensions/`; supporting modules, tests, and documentation live directly under `~/.pi/host/`, `~/.pi/tests/`, and `~/.pi/docs/`. Do not create a separate `twin` or `pi-runtime` project. Keep backups under `~/.pi/backups/`. Never add credentials, private memory, session history, or caches to source control.

```text
~/.pi/
  contracts/                 # Versioned schemas and generated bindings
  host/
    session/ jobs/ policy/ usage/ artifacts/
    agents/ workflows/ context/ memory/ refinement/
    providers/ verification/ supervisor/ experiments/
  python/pi_runtime/         # Typed client, execution environment, bounded rendering
  integrations/pi/           # Thin tools, commands, lifecycle adapter
  definitions/agents/        # Root, reviewers, verifier, tests-writer
  definitions/workflows/     # change-review, later implement-and-verify
  tests/unit/ integration/ subprocess/ recovery/ acceptance/
  evals/capabilities/        # Can each routed model use the typed RLM interface?
  evals/quality/             # Task outcomes and optional-feature ablations
  docs/adr/                  # Authority and compatibility decisions
  docs/acceptance.md
  scripts/                   # Check, gate, install-profile, rollback-profile
```

Migrate by responsibility, not a wholesale rewrite:

- `prime-rlm/registry.ts` -> shared jobs + task invocation; remove it as an authority after cutover.
- `prime-rlm/kernel.ts` and `runtime.py` -> bounded kernel service and Python API; adapt Prime protocol behavior after license/version review.
- Jev routing/memory/refinement helpers -> host services; keep thin compatibility wrappers.
- `workflow.ts` -> workflow service only after the common invocation contract works.
- Gandalf self-review -> versioned definitions/workflow; preserve old package until parity passes.
- Old registry/snapshot files -> read-only legacy evidence; no invented recovery guarantees.

New state stays in a versioned profile. Existing Pi history is never rewritten in place. Import/export must preserve original IDs/provenance or explicitly map them. New task/refinement entries must remain readable as opaque records by the legacy profile.

## 12. Phased delivery and exit gates

Each phase delivers code, tests, a small demonstration, updated acceptance rows, and a rollback procedure. No phase is complete on file existence alone.

### P0: freeze the contract and prove Pi integration feasibility

- Work directly in `~/.pi`; create private backups and a source manifest without placing credentials or session history into source control.
- Record local source hashes, upstream commits/versions, applicable licenses, and package inventory.
- Define acceptance cases and evidence formats independently of implementation, including the fast no-memory/no-graph path and the end-to-end demonstration in section 13.
- Confirm initial resource limits, schema strategy, state lifetimes, message delivery, and retention policy. Resolve mandatory ADRs for session authority, kernel reuse, isolation, provider metering, and Hindsight behavior.
- Freeze experiment variants, task partitions, memory/skill isolation, and acceptance thresholds before collecting candidate scores.
- Spike Pi SDK lifecycle/persistence, context interception before inference, all-call provider accounting, candidate-yield control, and detached UI feasibility.
- Compare using Prime's runtime code behind a narrow adapter against retaining the current kernel protocol. Select one kernel implementation, not two.
- Decide and document SDK integration versus minimal Pi host patch. No RLM brand claim substitutes for required hooks.

Exit: a real non-production session proves tool registration once, session replacement, branch interception, and controlled model dispatch; ADR names every missing hook and its solution. Stop if no honest single-authority path is established.

### P1: shared state, admission, artifacts, and bounded execution

- Implement schemas, event commits, derived state, job lifecycle, artifact store, cancellation, and execution profiles.
- Implement model admission/accounting for root and nested calls before expanding concurrency.
- Introduce scoped grants and workspace isolation without changing the default live profile.
- Implement branch ownership and checkpoint manifests; explicit failure where a checkpoint cannot restore.

Exit: real processes prove cancellation, output bounds, no late commits, duplicate-admission handling, concurrent budget reservations, and branch-safe restoration. Isolation tests must pass before enabling isolated mode.

### P2: RLM-first root and typed agent invocation

- Add data handles, bounded previews, final artifacts, context/event APIs, and typed result repair.
- Implement `agents.invoke`, `rlm.spawn`, await/collect/message/cancel through one task service.
- Add root, investigator, security-reviewer, and finding-verifier definitions, plus a small predict method using Jev where supported.
- Implement deterministic/predict/rlm strategies under one invocation contract, invocation-scoped scratch, versioned agent state, safe preview/describe, and nested context selection.
- Test recursive admission at saturation so waiting parents cannot deadlock their own descendants.
- Run model-facing capability evaluations for every configured routed model before assigning it methods that require those capabilities.
- Route both Python callbacks and retained direct tools through the same host services.
- Adapt Pi skill discovery into the bounded metadata catalog; record exact selections without granting capabilities.
- Implement the RLM-only fast path and capture a baseline experiment record before optional graphs are introduced.

Exit: a task larger than the model's window is processed through data references and programmatic child calls; the root receives bounded previews; output schema violations cannot reach a consumer as success. Explicit model choice and root pin are preserved. A self-contained task works with graphs and memory disabled and does not spawn a child or refiner merely because those services exist.

### P3: graph composition and Gandalf review parity

- Implement optional graph semantics and `change-review@1`.
- Adapt correctness/security/test reviews and skeptical verification from the existing package.
- Add host-owned check receipts, baseline/candidate comparison, immutable candidate capture, and changed-revision invalidation.
- Preserve `/self-review`, `/self-review-last`, `/test-writer`, and Jev judgment interfaces.
- Add `/skills why` and workflow inspection derived from selection/node events, not hand-maintained diagrams.

Exit: direct, graph, and recursive calls have identical definition/validation/policy semantics. A failing required check blocks completion. An incomplete reviewer never yields a clean report. Existing review functionality passes parity cases.

### P4: explainable memory and controlled continual adaptation

Deliver in three slices, all using the existing task/event services:

- **P4a, memory:** scoped operations, explainable `RetrievalPlan`, authorized query rewriting, ingestion reconciliation, correction/supersession, bounded consolidation, and `/memory` inspection/correction/forget commands.
- **P4b, procedures:** replace append-only refinement notes with versioned proposals; add skill/agent-promotion review, duplicate detection, coordination-pattern evidence, activation, and rollback commands. Keep workflow mutation gated until its additional tests pass.
- **P4c, measurement:** experiment history, frozen memory/skill variants, comparison views, and separate deterministic/quality outcomes.

Test rejected updates, stale base versions, branch-local lessons, protected instructions, rollback, contaminated skills, retrieval reuse, deletion propagation, and grants that remain unchanged by skill loading.

Exit: a verified correction becomes a scoped reusable improvement in a later task; obsolete information is identified as superseded; a rejected or rolled-back change does not remain active. `/memory why` explains the actual recorded retrieval without making a new search. A procedure proposal reaches activation only through its checks and required approval. Comparison history shows its later outcomes, including failures. Maintenance jobs do not block ordinary replies.

### P5: retained execution and long-running controls

- Add a local authenticated supervisor with restricted Unix socket and explicit attach/detach/stop semantics.
- Retain agent instances across invocations; implement checkpoint/archive/release/close and ownership-aware GC.
- Route parent/child/sibling messages with bounded queues, owner generations, IDs, safe delivery points, and verifier-context restrictions.
- Add `/agents` inspection, targeted correction, subtree stop, and retention controls; preserve existing aliases.
- Add goals, schedules, continuation budgets, progress receipts, and bounded reassessment using the existing job service.
- Add a composable candidate-yield controller only now, when goals, verification, and refinement need it. Journal its state; do not add another graph engine.
- Claim scheduled ticks before delivery; coalesce missed ticks; reconcile uncertain delivery rather than replaying blindly.

Exit: UI detachment preserves work; explicit stop kills it; crashes recover or report uncertainty honestly; schedules and retries do not create duplicate authorized effects; goals cannot bypass required gates.

### P6: acceptance, canary, and cutover

- Run the complete acceptance matrix, fault tests, model-facing capability tests, and held-out quality comparison, including RLM-only and isolated memory/graph/refinement ablations.
- Execute the combined demonstration in section 13 and its failure variants on the exact candidate build. Preserve all failures in the report; do not substitute a hand-scripted look-alike for real RLM dispatch.
- Verify memory and experiment views against canonical events, and confirm comparison runs cannot contaminate the daily memory bank.
- Canary in a separate Pi profile on read-only repository audits, then isolated local changes.
- Test startup, reload, new/resume/fork/tree, print/RPC, Cursor provider, Exa MCP, and command parity.
- Cut over only after user review of the evidence report. Remove legacy duplicate authorities/registrations in one controlled change.

Exit: all mandatory acceptance rows pass on the exact candidate version; no unknown safety/recovery row is labeled complete. Publish known limitations and restore commands.

Dependencies: P0 -> P1 -> P2 -> P3 -> P4 -> P5 -> P6. After P1, Hindsight API investigation and review-definition drafting may run in parallel, but activation waits for their phase gates.

Milestones:

- **M1, P2: RLM core usable.** Persistent data, programmatic typed delegation, bounded outputs, and a direct-answer path work without graphs or memory.
- **M2, P3: composition usable.** The same security reviewer runs directly, recursively, and through change-review, with shared policy and commit-bound receipts.
- **M3, P4: adaptation inspectable.** Memory decisions, corrections, skill proposals, activation, rollback, and comparison history work through the same recorded services.
- **Full integration, P6:** retained execution, fault recovery, compatibility, canary evidence, and rollback are proven. Earlier milestones must not be labeled complete synthesis.

### First implementation slices and work ownership

Do not start by expanding `workflow.ts` or adding new prompts. Start with these bounded changes:

1. **Contract and fixtures:** define TaskHandle/TaskResult, AgentDefinition/MethodDefinition, event ownership, and an immutable large-input fixture. Implementer and independent acceptance reviewer agree on observable behavior before coding.
2. **Pi feasibility spike:** prove one no-op typed invocation can pass through Pi SDK, commit its lifecycle, survive session replacement, and be inspected. Record missing hooks instead of adding parallel state stores.
3. **Bounded execution slice:** run one Python cell in a controlled worker with output cap, deadline, artifact spill, cancellation, and a terminal record. Test actual process termination.
4. **Typed recursive slice:** invoke one specialist from Python through shared admission and metering; validate its result; prove admission is not completion.
5. **State/context slice:** add invocation scratch, retained instance identity, checkpoint manifest, bounded inspection, and restricted reviewer context.

Use three implementation responsibilities, not three competing runtimes:

- Builder implements a bounded slice from the public contract.
- Acceptance reviewer owns cases/evidence and checks coverage independently; model judgments remain advisory.
- Integrator owns compatibility, migration, and the full acceptance matrix, including unimplemented rows.

Before code merges into the canary profile, require the narrow baseline, candidate run, regression evidence, and updated acceptance status. No calendar estimates until P0 exposes the actual Pi integration work. Definitions and UI can be drafted in parallel; lifecycle and schema ownership changes need a single integrator.

## 13. Acceptance matrix

All cases start unverified. Test names below are planned identifiers, not existing tests. Each row records command, fixture/version, expected behavior, actual outcome, artifacts, and reviewer judgment.

| ID | Public behavior to prove | Planned evidence |
|---|---|---|
| A01 | Direct/graph/RLM invocation shares one definition and result contract | `invocation_parity` integration suite |
| A02 | Large input stays outside active context; full data remains usable | Large-data fixture + captured request sizes + expected output |
| A03 | Invalid result triggers bounded repair or typed failure | Malformed and semantically invalid result fixtures |
| A04 | Root pin and explicit child model are preserved | Provider request capture and live smoke |
| A05 | Cancellation terminates owned work and rejects late success | Real process tree, delayed provider, race fixtures |
| A06 | Output, artifact, wall-time, concurrency, and process limits hold | Flooding/hanging worker tests under each supported profile |
| A07 | Branch/fork/resume restores matching state; no abandoned lesson leak | Lifecycle sequence tests across restart |
| A08 | Unsupported Python state reports non-restorable, never silently correct | Snapshot type matrix and tamper rejection |
| A09 | Concurrent calls/retries cannot bypass tree budget admission | Transaction races, missing usage, unknown pricing fixtures |
| A10 | Worker receives no ambient credentials or unauthorized host capability | Environment inspection and isolated capability-denial tests |
| A11 | Approval binds scope/revision/policy and is rechecked | Revocation, expiry, changed-base, wrong-owner fixtures |
| A12 | Graph validates before effects; routes/skips/fan-in/cycles are explicit | Deterministic graph contract suite |
| A13 | Agent admission does not satisfy a graph dependency | Slow child with delayed valid result |
| A14 | No checks/incomplete review/stale receipt cannot imply verified completion | Check and review negative fixtures |
| A15 | Baseline versus candidate is recorded; required regressions block | Independent reference fixtures, protected checker inputs |
| A16 | Memory skip causes no retrieval; scopes survive consolidation | API capture and real backend contract tests |
| A17 | Correction supersedes stale claims; hypotheses remain distinguishable | Contradictory Pi-launcher scenario across fresh sessions |
| A18 | Ingestion acceptance is distinct from completed retention; forgetting persists | Backend failure/restart/deletion contract tests |
| A19 | Refiner cannot alter policy, grants, or acceptance tests | Capability-escalation and policy-edit proposals |
| A20 | Versioned refinement changes later behavior and can roll back | Before/after/rollback task with independent expected behavior |
| A21 | Replay does not duplicate external effects; uncertainty stays explicit | Lost-response fake service plus crash-boundary tests |
| A22 | Detached work and retained children survive UI loss | Real supervisor/client disconnect/reconnect test |
| A23 | Schedules/goals/verification compose within budgets | Duplicate tick, missed tick, goal-pause, required-gate tests |
| A24 | Costs, task transitions, and inspection agree on one record | Event reconstruction and accounting reconciliation |
| A25 | Existing Pi capabilities and data survive migration and rollback | Profile parity, import/export, backup restoration rehearsal |
| A26 | Component changes cannot pass merely by weakening tests | Independent acceptance instrument review and regression mutation slice |
| A27 | RLM handles a self-contained task without graph/delegation/memory/refinement | Runtime test with optional services disabled and no supporting dispatches |
| A28 | Retrieval explanation/query/scope match the executed search; rewrite cannot widen access | Recorded decision, query rewrite failure, scope-denial, and retrieval-reuse tests |
| A29 | Memory inspector shows actual injected evidence without a new search or hidden-scope disclosure | Command/API parity, authorization, provenance, and no-fetch inspection tests |
| A30 | Correction/deletion propagates to managed memory views/exports without changing trusted instructions | Backend completion, cache invalidation, export, and AGENTS.md preservation tests |
| A31 | Skill selection is version-pinned, explainable, and cannot grant capabilities | Discovery/load/invoke tests with restrictive grants and concurrent activation |
| A32 | Skill proposals require evidence, validation, and applicable approval; rejection/rollback holds | Repeated-procedure, duplicate, invalid-skill, rejection, and rollback cases |
| A33 | Experiment history includes all attempts and variants do not contaminate each other | Frozen config/fixtures, memory isolation, failed-run recording, usage reconciliation |
| A34 | Optional-service failure preserves RLM operation without bypassing required checks or replaying effects | Memory outage, unavailable workflow, partial-effect, and required-verification failure tests |
| A35 | Deterministic/predict/rlm methods share policy, validation, accounting, and task semantics | Strategy parity tests; predict launches no unnecessary iterative kernel |
| A36 | Invocation scratch is isolated while declared instance state persists | Sequential and concurrent calls, nested same-instance calls, restart fixtures |
| A37 | Bounded preview preserves full values and does not execute unsafe introspection on the host | Large values, cyclic objects, hostile repr, and artifact range-read tests |
| A38 | Nested context excludes unauthorized and contaminating history | Captured reviewer requests and scope-restricted event queries |
| A39 | Configured models can use the actual typed RLM interface | Versioned live capability suite with repeated trials and objective outputs |
| A40 | Retained instances accept new invocations without rewriting old terminal outcomes | Complete/retain/follow-up/close lifecycle and restart sequence |
| A41 | Family messages enforce identity, scope, deduplication, expiry, and safe delivery | Forged sender, cross-family, saturation, replay, steering, and verifier-isolation tests |
| A42 | Human child intervention and subtree stop preserve unrelated work | Two-child run with inspect/correct/stop operations and evidence readback |
| A43 | Resource release/eviction respects live references, evidence retention, and quotas | Pinned checkpoint, live artifact, idle eviction, and deletion-policy tests |
| A44 | Progress and reassessment use evidence and cannot extend budgets or declare unsupported success | Stalled/busy/progressing fixtures and unknown-verifier cases |
| A45 | Saturated nested recursion makes progress or returns bounded capacity failure, never deadlock | Parent-awaiting-child resource saturation and cancellation tests |
| A46 | The combined system performs the full demonstration without manual state repair | Real RLM run plus crash, validation, scope, and refinement failure variants |

Deterministic mandatory cases require 100% pass. An unexpected test-runner timeout or crash is an infrastructure outcome, not a behavioral pass or mutation kill. Deliberately timing out/killing a worker is a valid fault test only when the supervising test completes and asserts the required lifecycle, cleanup, and recovery behavior. Use Jev test-quality/mutation judges as review evidence only, following observed baseline runs.

Every acceptance row must have a phase owner before implementation: A01-A11 and A27/A35-A38/A45 primarily P1-P2; A12-A15 and A31 primarily P3; A16-A20 and A28-A30/A32-A33 primarily P4; A21-A23 and A40-A44 primarily P5; A24-A26/A34/A39/A46 are cross-cutting and release-gated in P6. Earlier phases establish the foundations for later fault tests. A row is unverified, passed, failed, or blocked with evidence; partial implementation is never a pass.

Maintain a separately reviewed acceptance instrument that worker profiles cannot modify. In trusted-local mode this is a process convention, not a security boundary; hostile-code evaluation requires actual isolation. Authorized requirement changes can revise the instrument through a recorded review, never silently to accommodate the candidate.

Quality comparison: freeze at least 20 varied tasks covering small edits, multi-file fixes, large-data research, review, and repeated-correction learning. Run baseline and candidate with matched models/settings, record all attempts, use repeated runs for stochastic outcomes, and predeclare quality/cost/latency thresholds before measuring. Keep memory, graph, routing, and refinement ablations separate from the main comparison. Use the experiment ledger in section 10 and prevent cross-variant memory/skill contamination. Passing runtime contracts does not imply better coding quality, and a small sample does not establish broad benchmark superiority.

### Model-facing capability suite

Evaluate each configured model/strategy combination using the real schemas, prompts, Python API, and host dispatch. Cases cover large-object inspection, exact typed returns, validation repair, programmatic fan-out, event retrieval, scoped context, and honest handling of a failed child. Use independently calculated expected outputs where possible, not the model's description of what it did.

P0 freezes trial counts and capability thresholds before candidate runs. Report correct outcomes, repair counts, latency, and total spend separately. A model that fails a required capability is removed from that method's eligible route set until explicitly requalified; do not silently substitute another model on an explicit-model task. Keep model competence results distinct from deterministic runtime correctness.

### Combined demonstration: fix, review, retain, improve

Use a disposable repository with a real multi-file defect, a pinned baseline/candidate, an oversized diagnostic dataset, and acceptance tests controlled outside the worker. The known solution and verification oracle must not depend on implementation-specific helper names.

1. The root RLM receives a concise task and dataset handle. It inspects bounded previews, computes over the full data, and chooses its own decomposition.
2. It invokes a small predict method where useful and an RLM investigator through the same typed task service. Jev routes eligible non-explicit selections; every call uses the root budget.
3. It produces a candidate in an isolated workspace and invokes `change-review` voluntarily, or directly invokes the same reviewer. Both paths produce equivalent contract-level evidence.
4. Reviewers receive scoped evidence, not hidden acceptance cases. The host runs baseline and candidate checks, and returns commit-bound receipts. A required failure causes bounded repair, not an unverified success claim.
5. Retain a reviewer. Detach and reattach the client; inspect that child, send a correction, and run a new invocation under its existing instance. The old result remains unchanged.
6. Stop a separate slow child while other work continues. Release unneeded data/idle computation without deleting required evidence.
7. Present the verified local artifact and limitations. No GitHub publication or deployment is required for this demonstration.
8. Propose a scoped skill/agent refinement from the observed failure. Validate and review it, then run a separate held-out follow-up task. Compare baseline, activated, and rolled-back versions using isolated experiment state.
9. Use `/memory why`, `/skills why`, `/agents`, and `/experiments` to reconstruct the decisions and costs from the same records.

Failure variants: invalid typed result; Jev/Hindsight outage; expired grant; stale revision receipt; exhausted shared budget; crash after admission; lost external-effect response against a disposable test service; cross-family message; non-restorable checkpoint; harmful refinement; and deletion of a referenced memory. None may be represented as verified completion. Some variants should deliberately stop rather than recover.

P6 passes A46 only when both the live demonstration and its deterministic failure variants have evidence. A video or attractive TUI is not sufficient. A successful demonstration does not replace the full matrix or held-out quality evaluation.

## 14. Verification command contract

P0 creates these project scripts; they do not exist yet:

```bash
npm ci
uv sync --frozen --project python
npm run check                 # TypeScript, schema/binding parity, lint
npm test                      # Deterministic unit and integration tests
npm run test:subprocess        # Real process and cancellation boundaries
npm run test:recovery          # Crash, branch, replay, and supervisor tests
npm run test:acceptance        # Complete A01-A46 report; unavailable != passed
npm run eval:capabilities      # Explicitly metered model/API capability suite
npm run demo:integration       # Disposable end-to-end run with evidence bundle
npm run eval:quality           # Explicitly metered live quality suite
npm run gate                  # Required suites plus frozen quality thresholds
```

Offline fixtures run without production services. Live suites require explicit credentials, a spend cap, and disposable targets. Never run unknown repository tests on the trusted host merely to evaluate this runtime.

## 15. Rollout and rollback

- Develop against a separate versioned test profile, not the active `~/.pi/agent` configuration.
- Back up resource manifests/config locally with restrictive permissions; do not dump credentials into logs or commits.
- Record every installed source version and migration version. Register each command/tool once.
- Before cutover, quiesce or explicitly stop managed jobs, reconcile external effects and outstanding spend, and checkpoint what is restorable.
- Roll back by restoring the prior resource/settings manifest and launching the preserved stock Pi with the old profile. Do not redirect `pi` to Prime. A custom Pi host build needs an explicit opt-in install and a preserved stock executable.
- Keep new journals/artifacts as read-only evidence. Do not feed incompatible snapshots to the old runtime or rewind external effects.
- Rollback removes active refinements by version pointer, not by erasing evidence. Hindsight writes already made require explicit compensating correction/deletion if appropriate.

## 16. Non-goals and scope controls

No multi-tenant product, Slack bot, cloud deployment automation, weight training, fleet scheduler, new scanner platform, or knowledge-graph database. No claim of arbitrary Python replay, universal exactly-once effects, or protection from hostile code in trusted-local mode.

Defer cosmetic dashboards, Python class-authoring conveniences, speculative compaction, general workflow mutation, and automatic plateau tuning until their core contracts and evaluation cases pass. Do not defer shared admission, cancellation, output bounds, state lifetimes, or result validation; they are prerequisites, not polishing work.

Main risks and responses:

| Risk | Required response |
|---|---|
| Pi extension API cannot own durable lifecycle | P0 ADR and minimal host integration; do not hide dual authority |
| Feature growth delays an RLM core | Deliver M1 first; graphs, memory UI, and scheduler remain later consumers |
| Upstream behavior/version changes | Pin source/contracts, test adapters, preserve rollback profile |
| Jev/provider outage or unexpected charges | Explicit failure policies, shared admission, unresolved usage accounting |
| State loss or corrupted snapshot | Versioned checkpoints, integrity/ownership checks, explicit non-restorable state |
| Refinement preserves a bad strategy or exploit | Independent evidence, scoped proposals, protected policy, rollback and holdouts |
| Security claims exceed isolation | Profile-specific tests and visible trust boundary; refuse unsupported isolated execution |

Do not implement every upstream feature. The required synthesis is the shared contracts and their observable guarantees. A graph, dashboard, extra tool, or package installation cannot substitute for a missing guarantee.

## 17. Research and local references

Primary sources read for this plan:

- Waku architecture: https://github.com/shenseanchen/waku-agent/blob/main/docs/architecture.md
- Waku tour, memory UI and procedural skills: https://github.com/shenseanchen/waku-agent/blob/main/docs/tour.md
- Waku retrieval gate: https://github.com/shenseanchen/waku-agent/blob/main/waku/memory/retrieval_gate.py
- Waku graphs: https://github.com/shenseanchen/waku-agent/blob/main/docs/agent-graphs-design.md
- Waku evals: https://github.com/shenseanchen/waku-agent/blob/main/docs/evals.md
- NOOA: https://github.com/nvidia-nemo/labs-OO-Agents
- NOOA paper, especially sections 2-3: https://arxiv.org/abs/2607.20709
- RLM paper, section 2: https://arxiv.org/abs/2512.24601
- Continual Harness, sections 2-3 and 6: https://arxiv.org/abs/2605.09998
- Prime Agent README: https://github.com/PrimeIntellect-ai/prime-agent
- Prime Agent paper, section 2 and refinement safety example in 3.5: https://arxiv.org/abs/2608.23552
- Stencil Harness Playbook: https://stencil.so/blog/harness-playbook
- Pi installed SDK/extensions/session/compaction docs under `~/.local/share/mise/installs/pi/0.84.4/pi/docs/`.
- Gandalf: `gandalf/swe/{gateway.py,publish.py,review.py}`, `swe/jobs/{states.py,outbox.py}`, `swe/worker/{ownership.py,broker.py}`, `swe/intake/confirmation.py`, `swe/verify/checks.py`, `memory/consolidation.py`, `observability/redaction.py`.
- Gandalf limitations/evidence: `docs/swe-platform/{acceptance-registry.md,p9-pilot-report.md}`.

Upstream main-branch descriptions and papers are design references, not proof of installed behavior. Pin exact revisions in P0. Do not transfer their benchmark results or production claims to this implementation.
