### Feature

**You own the design. Plan, review, verify.** Delegate implementation; stay in the lead.

1. `../how/SKILL.md` over the affected subsystem.
2. `../architect/SKILL.md` for parallel design exploration.
3. Write the throughput checkpoint as four plan items. A dimension that does not apply keeps its item as
   `n/a: <reason>`:
   - **Blocking first steps.** Gates that run before fan-out.
   - **Independent workstreams.** Disjoint files, services or layers parallelize; shared writes serialize.
   - **Shared mutable state.** Default to splitting the target (`the separate-before-serializing-shared-state principle (`../principle-separate-before-serializing-shared-state/SKILL.md`)`).
     Serialize only for real invariants.
   - **Smallest safe decomposition.** If one worker is best, say why.
4. Delegate the code to a child: `rlm.spawn(brief, name=..., worktree=True, model=state["models"]["code"])`, or
   `hard` for cross-cutting or subtle work. The brief names file paths, the data shape and its organizing structure
   (`the model-the-domain principle (`../principle-model-the-domain/SKILL.md`)`: a state machine over scattered booleans, a table over branching, a typed model over
   repeated shape assumptions), chosen before the child writes logic, and success criteria. When the implementation
   admits several valid shapes (error handling, abstraction layer, test structure), delegate through
   `../arena/SKILL.md` instead. Delegation is mandatory: the gain is review separation, not lines saved, and the
   laziness protocol does not override it. A child without spawn depth satisfies it by owning the diff, reviewed by its
   parent. Comments per **Comments** in `SKILL.md`. Surgical edits; re-ground upstream-derived files against their
   source. Port shared-primitive improvements to every consumer and verify each. Commit liberally.
5. Verify on the matching surface. Inconclusive or wrong-surface is not a pass; flag it.
6. Rebase into small, ordered commits; stack follow-ups (`the sequence-verifiable-units principle (`../principle-sequence-verifiable-units/SKILL.md`)`).
7. If the design is contested, `../interrogate/SKILL.md` before shipping.
8. Run `playbooks/opening-a-pr.md`.

Code-coupled work (one feature, one migration) goes to a single owner child with the checkpoint in its brief and
`depth=1`, so it fans out after its blocking phase. Parent-level fan-out is for slices that produce independent
artifacts (audits, cross-subsystem investigations, competing experiments). Rewrite the checkpoint at phase boundaries.

**Reply:** what you built, what you chose and why, the throughput checkpoint, open decisions. Tables for design
alternatives.
