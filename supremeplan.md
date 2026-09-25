# Ultron Supreme Plan

Ultron's promise is a coding agent that *programs over its work* instead of reading it: a persistent
Python environment where the model writes code, delegates bounded sub-inferences, and keeps large
inputs out of its own context. Today it has the machinery (persistent kernels, typed agents,
workflows, Jev-gated memory, a durable task journal, budgets) but ties stock Pi on every eval,
because the harness still offers the model the easy path: `bash`, `write`, and reading files into
context. Models take the easy path 16 times out of 16.

This plan closes that gap. It is ordered so each phase is independently shippable and measurable.

Sources it draws on, and what each contributes:

| Source | Idea taken | Where it lands |
|---|---|---|
| Prime Intellect nano-rlm (via `verifiers` RLM harness) | REPL as the sole tool; shell/edit as pre-imported skills; 20 KB middle truncation; delegation prompt | Phase 1 |
| NVIDIA NOOA (paper 2607.20709, "six harness capabilities") | Pass by reference (bounded previews); model-callable context/event APIs; typed I/O with retry; agents as Python classes | Phases 1, 3, 6 |
| LLM-as-Code (arXiv 2606.15874) | Program owns control flow, LLM calls are leaves; DAG context that collapses on return; self-programmed evolution committed as tested code | Phases 3, 4 |
| Autolith (lambda-symbolics) | `rlm-complete`: the root never sees the input; `infer` frames with contracts + repair; shared budget subtree with tranches; distilled decomposition policies; generations with rollback | Phases 2, 5, 6 |
| waku-agent | Retrieval gate (have it: Jev); graph workflows (have it); LLM-judge eval tier next to deterministic checks; legibility dashboard | Phase 5 |
| unreal-agent | Versioned sessions/operations that fail explicitly on resume; serializable operations | Phase 5 (hygiene) |

The one sentence all six agree on: **keep the big stuff out of the model's context and make the
model program over handles.**

---

## Current state (2026-09-26)

- Branch `ultron` = local `main` at `f87c8e31a`. Acceptance A01–A46: 46/46. Mutation slice 10/10.
  Full `./test.sh` green.
- Runtime: native RLM session worker; Pi UX (TUI, `-p`, `--mode json`, `--mode rpc`) at parity.
- Host API reachable from the kernel (all async, top-level `await`):
  `rlm.spawn/collect/list_subagents`, `agents.list/register/invoke/spawn/result/inspect/cancel/status`,
  `workflows.run`, `memory.prepare/propose/correct/forget/why`, `refinements.*`, `skills.*`,
  `background.start/list/inspect/result/stop`, `jev.triage/recall/call`, `progress.*`, `experiments.*`.
- Evals: `scripts/eval-quality.mjs`, default set (20 tasks) and hard set (15 tasks,
  `evals/quality/tasks-hard.mjs`). Latest hard result, glm-5.3-flash at `--thinking max`:
  Pi 29/30, Ultron 29/30, Ultron 0.90x latency. `rlm` tool used in 0/16 runs of a data task.
- In progress (branch `ultron-rlm-only`, agent running): Phase 1.

---

## Phase 1 — The REPL is the only tool  *(in progress)*

**Goal.** The root model's default tool set is `rlm` alone. Shell, edit, read, write are Python
functions inside the kernel. Big outputs cannot flood the context.

**Design (copied from nano-rlm, adapted to Ultron's API).**
- `session-worker.ts`: default tools = `[rlm, ...extensionTools]`. Native `read/edit/write/bash`
  only with `ULTRON_TOOLS=native` or explicit `--tools`. Child task lanes get the same set.
- `runtime.py` skills, pre-imported: `await bash('''cmd''')` → output string (str subclass with
  `.exit_code/.stdout/.stderr`); `await edit(path, old_str, new_str)` → exactly one occurrence or
  `ValueError` (absent / ambiguous); `read(path)`, `write(path, text)` thin helpers; `background.*`
  for jobs.
- Output: middle truncation of every rlm result at 20 KB (`ULTRON_RLM_MAX_OUTPUT_BYTES`) with an
  elision marker; last-expression values render as bounded previews (type, length, head/tail, digest)
  and stay in the kernel (`preview(x)` for more).
- Prompt: a "Runtime" section in the system prompt in the spirit of nano-rlm's `RUNTIME_PROMPT`:
  persistent kernel, pre-imported APIs, `await`, handles, one line per skill, packages available,
  "run project code through bash with the project's interpreter", and a short delegation section
  (when to spawn, how to brief, `await rlm.collect()`).
- Eval: records `toolsByName` per run so uptake is measured, not assumed.

**Acceptance.**
- Tool list test: `[rlm]` + extension tools by default; opt-out restores native set.
- Skill tests: bash/edit/read/write semantics including edit's two error cases.
- Truncation and preview tests.
- Scripted-provider CLI test: the model edits a file through `rlm` using `edit`.
- Hard-set comparison on glm-5.3-flash at max: Ultron ≥ Pi pass rate, `rlm` used in ≥ 90% of runs,
  and no regression on the default set.

**Risks.** Models trained on tool-calling may fumble Python quoting; the prompt's triple-quote rule and
the `edit` error messages are the mitigation. Extension tools (MCP) must keep working; test covers it.

---

## Phase 2 — Bounded inference: `rlm.infer`, `rlm.map`, `rlm.load`

**Goal.** The piece that makes Ultron *beat* Pi on large-data and forensics tasks: the root never
reads a large input; it slices it in code and delegates bounded sub-inferences that return validated
values. This is Autolith's `infer`/`rlm-complete` and the RLM paper's defining property.

**Design.**
- `h = rlm.load(path | text | bytes)` → `ContextHandle` with `.label, .size, .digest`; the content is
  interned in the kernel (and content-addressed on disk under the session), never returned. Methods:
  `h.length()`, `h.slice(a, b)`, `h.search(pattern, limit)`, `h.lines(a, b)`, `h.chunks(n)`.
  Only slices enter the model's context, and only when printed.
- `v = await rlm.infer(task, context=[h.slice(...), "literal", other_h], contract=schema | None,
  budget=Budget(calls, tokens, depth), model=None)` → an *inference frame*: a private, ephemeral
  conversation seeded with the task and the materialized context views; no parent transcript, no
  tools except a read-only `rlm` cell over its own views (depth permitting). Returns the contract-
  validated value (JSON-schema, tagged native data) or a string. Malformed answers are re-asked within
  the remaining budget (Autolith repair). On exhaustion returns an `Incomplete` observation: status,
  spent/remaining budget, trace id, last bounded outputs — evidence, not a failure.
- `vs = await rlm.map(tasks, context=..., contract=..., budget=...)` fans frames out under one budget
  subtree, order-preserving, per-item `Incomplete`/`Error` entries.
- Implementation: frames are `agents.invoke` on a built-in `rlm-frame` definition (strategy `rlm`,
  `outputSchema` = contract) so they inherit the task journal, cancellation cascade (A42), usage
  reservations and Jev routing. New host requests: `rlm.load`, `rlm.infer`, `rlm.map`. Frame traces
  are stored as session values `ultron.rlm.frames/<id>` and viewable in `/rlm`.
- Budget subtree (Autolith): every frame carries `{calls, tokens, depth}`; each provider request
  reserves one call and an output-token tranche (≤ ¼ of the remaining pool, ≤ 16k), settles against
  reported usage, refunds the tranche on failure, discounts cache reads. Concurrent siblings cannot
  oversubscribe. Extends `NativeUsageLedger.reserve/settle` with a `budgetId` and tree accounting.
- Prompt: the Runtime section explains handles and frames with two examples (log forensics; huge CSV).

**Acceptance (new rows A47–A50).**
- A47: an input larger than the model's context window is processed to the exact answer with no single
  request exceeding the window and the root transcript never containing the input (scripted provider
  asserts every request body).
- A48: contract repair: a frame that first answers malformed JSON is re-asked and returns a valid value;
  exhaustion yields `Incomplete` with evidence; the root turn continues.
- A49: budget subtree: a `map` of 8 frames under `calls=6` runs 6 and marks 2 incomplete; total tokens
  never exceed the pool; refunds on provider failure.
- A50: cancellation: aborting the root cancels every running frame within 2 s (reuses A42 harness).
- Eval: hard-set `data-*`, `logs-*`, `huge-*` categories: Ultron pass rate > Pi's and median tokens per
  task < Pi's (this is where the win should show).

---

## Phase 3 — The model owns its context

**Goal.** NOOA capability #6 (model-callable context/event APIs, +11.8 on ARC-AGI-3) and LLM-as-Code's
collapse-on-return, so long sessions stop dragging stale results.

**Design.**
- Kernel namespace `ctx`: `ctx.history(limit, kinds)` → bounded list of transcript items with ids and
  sizes; `ctx.get(id)`; `ctx.forget(ids, reason)` removes items from the *model's* context (a Pi context
  edit on the branch; the transcript stays durable); `ctx.summarize(ids, text)` replaces a span with a
  model-written summary; `ctx.pin(id)` protects from compaction; `ctx.note(text)` appends a durable
  note visible after compaction. All are branch-anchored like refinements.
- Collapse on return: when a task or frame completes, the root transcript keeps a one-line summary
  (definition, key, status, cost, `result` handle); the full result lives in the kernel as a variable
  and in the journal. Implemented as an automatic context edit emitted by the host on `task_end`.
- Compaction integration: Ultron's compaction consults pins and notes; the `/rlm` panel shows what the
  model has forgotten or pinned.

**Acceptance.** A51: after 20 completed tasks the root's next request is bounded (assert request size
in a scripted run) while `agents.result` still returns full values. A52: `ctx.forget` never removes
the current user turn or pinned items; an extension can observe the edit. Eval: default set latency and
cost ≤ Pi's on 10-turn multi-step tasks.

---

## Phase 4 — Procedural memory as tested code

**Goal.** LLM-as-Code's "self-programmed evolution" and waku's procedural pillar, on Ultron's
refinement machinery: a decomposition that worked becomes a Python skill with a test, promoted only
when the test passes, versioned with rollback.

**Design.**
- `skills.propose_code(name, source, test_source, evidence)` writes `<agentDir>/skills/<name>/`
  (`skill.py`, `test_skill.py`, `SKILL.md`), runs the test in a fresh kernel, records the outcome in the
  journal, and activates only on pass. `skills.rollback(name, version)` as for refinements.
- Active code skills are importable in every kernel (`from skills import <name>`); their docstrings are
  listed in the Runtime section (bounded), so the model reuses them instead of re-deriving.
- Jev scores proposals (relevance, sensitivity) as it does for memory; the tool-round nudger suggests
  extracting a skill after a long successful streak.
- Distilled decomposition policies (Autolith): a successful `rlm.map` plan is recorded as a policy skill
  (`plan(handle) -> [tasks]`), replayable on the same task family.

**Acceptance.** A53: a proposed skill whose test fails is never importable; a passing one is, survives
worker restart, and rolls back cleanly. A54: on a repeated hard task the second run uses the skill
(toolsByName + skill import observed) and costs less than the first.

---

## Phase 5 — Measure like Prime, keep like Unreal

- **LLM-judge eval tier** (waku): for tasks without an exact checker (refactors, explanations), a judge
  with a frozen rubric and a fixed model, reported next to deterministic checks, never used alone for the
  release gate. Extend `eval-quality.mjs` with `judge: {model, rubric, score}`.
- **Uptake metrics** in every eval record: `toolsByName`, frames spawned, tokens the root *did not* see
  (sum of handle sizes minus slices printed). The plan's thesis is falsifiable through these numbers.
- **Versioned sessions and operations** (unreal-agent invariant): session values and journal entries
  carry a format version; an unsupported version fails explicitly on resume with the version named.
- **Budget tranches** (if not fully done in Phase 2) and per-root `max_total_tokens` /
  `max_total_turns` mirroring nano-rlm's policy knobs, exposed as settings.

---

## Phase 6 — Agents as Python classes

**Goal.** NOOA's core ergonomics inside Ultron's kernel: the model (or the user) defines an agent by
writing a class; docstring = prompt, typed methods = capabilities, `...` methods = LLM-driven, fields =
durable state. It replaces hand-written JSON definitions with something the model can author in one
cell.

**Design.** `@agent` decorator in `agents_api.py` derives a `NativeDefinitionDescriptor` (input/output
schemas from annotations, strategy `rlm`) and registers it via `agents.register`. `...` methods become
`agents.invoke` calls with the method's typed contract; real bodies run in the kernel. Instances map to
Ultron `instances` (state survives `reset_scratch`). Generations with rollback (Autolith) apply through
the definition registry's version hashes.

**Acceptance.** A55: a class defined in a cell is invokable typed end to end, its state persists across
cells and snapshots, a bad return is rejected with the schema error (A14 no-false-verification holds).

---

## Sequencing and gates

| Phase | Depends on | Ships when | Metric that proves it |
|---|---|---|---|
| 1 REPL-only | — | tests + hard eval ≥ Pi, `rlm` ≥ 90% uptake | pass rate, toolsByName |
| 2 infer/map/load | 1 | A47–A50 + data/logs/huge categories > Pi | pass rate, tokens the root never saw |
| 3 context APIs | 1 | A51–A52 | request size over 20 tasks, cost |
| 4 code skills | 1, 2 | A53–A54 | second-run cost |
| 5 measurement | 1 | judge tier + versioning + metrics | — |
| 6 agent classes | 2 | A55 | authoring time, definition count written by the model |

Every phase: `npm run check`, `./test.sh`, `npm run test:acceptance`, mutation slice, `acceptance:lock`,
rebuild, fast-forward `main`. Metered comparisons run on glm-5.3-flash at `--thinking max` until
gpt-6-sol's cooldown ends (~2026-09-27), then both.

## Non-goals

- A sandbox. Model code runs as the user by choice; the snapshot key is therefore readable by it.
- Autolith-style live self-modification of Ultron's own TypeScript. Self-modification is scoped to
  Python skills and policies with tests and rollback.
- Unreal's remote operation manager. Operations stay local.
- Reintroducing native `bash`/`write` as defaults. If a model cannot work through the REPL, that is a
  model-routing decision for Jev, not a reason to reopen the escape hatch.
