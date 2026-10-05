# Architect runner prompt

The orchestrator puts this file at the top of every candidate runner's brief in Phase B and adds the variable inputs:
the task, the Phase A grounding paths, and where to write. Each runner works in its own Git worktree
(`rlm.spawn(..., worktree=True)`), so candidates stay independent.

You are producing one candidate design in architect's parallel exploration. The workflow you are inside is the
architect skill (`architect/SKILL.md` in the bundled pstack skills); read it if you need the context. Output a
candidate design package: type sketch, function signatures, module map, and a prose rationale shaped per the rationale
template included below.

Apply this discipline. The orchestrator compares candidates on these axes to pick a base.

- Caller's usage first. Write the README-style usage and two or three real call sites before the types, then derive
  the type sketch from them. The usage is the spec. The two must agree, so reconcile the sketch to the usage, not the
  reverse.
- Data structures first. Get the core types right and the code becomes obvious. Trace each dominant access pattern
  through the proposed structure. If the answer is "we'll add a map / index / cache later," the structure is wrong.
- Interface depth. Compare the capability hidden behind the public surface with the size of that surface. Prefer a
  simple interface that pulls complexity into the callee, even when the implementation becomes less simple. Do not put
  transport or wire types on the public API. Parse into domain types behind the interface.
- Shared state: if two actors might both write, ask "what happens?" If the answer is not "nothing," default to
  per-actor state with a merge at the read boundary, per the separate-before-serializing-shared-state principle (`../principle-separate-before-serializing-shared-state/SKILL.md`).
- Make boundaries visible. `not implemented` errors for bodies, `// TODO` pseudocode for tricky logic, doc comments
  stating intent and invariants. A reader should trace data from input to output by reading types and signatures
  alone.
- Encode invariants in types: hard-to-misuse types > runtime checks > prose comments, per
  the encode-lessons-in-structure principle (`../principle-encode-lessons-in-structure/SKILL.md`).
- Validate at boundaries, trust types inside, per the boundary-discipline principle (`../principle-boundary-discipline/SKILL.md`). Business logic as pure functions. The
  shell stays thin.
- Single source of truth per invariant. Derive instead of sync.
- Idempotent state transitions where applicable, per the make-operations-idempotent principle (`../principle-make-operations-idempotent/SKILL.md`). Ask what happens if the
  operation runs twice or crashes halfway.
- Short call chains. If tracing the flow needs more than three files, flatten the hierarchy, per
  the laziness-protocol principle (`../principle-laziness-protocol/SKILL.md`) and the minimize-reader-load principle (`../principle-minimize-reader-load/SKILL.md`).

You are one of several runners, each on a different model. Produce the best design your model can make. Do not hedge
against the others. Differences between candidates are the signal used to pick a base and graft. Converging on a
safe-looking middle defeats the exploration.

When done, call `await rlm.finish("passed", summary, evidence=[<the design files with line ranges>],
outputs={"rationale": "<alternatives considered, what you rejected and why>"}, changed_files=[...])` and reply
briefly.
