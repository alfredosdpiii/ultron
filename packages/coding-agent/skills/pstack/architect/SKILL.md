---
name: architect
description: "Sketch types, signatures and module structure before code, run an arena of structurally distinct designs, then implement against the chosen sketch and scrap it when it proves wrong. Use for /skill:architect, 'architect this', 'design this', or non-trivial work where jumping to code would lock in the wrong shape."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/architect
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Architect

Design before implementing. Sketch types, function signatures, class shapes and module boundaries with
`not implemented` bodies and pseudocode. Synthesize across several model perspectives, then fill in code against the
chosen sketch. If implementation proves the sketch wrong, throw it out and redesign.

```python
state["plan"] = ["ground", "sketch (arena)", "agree", "implement", "scrap if wrong"]
```

## Phase A: Ground the problem

Build a real mental model of every system the new code touches: read `../how/SKILL.md` and run it over the relevant
subsystems. Naming a file is not grounding; produce the traced model `how` prescribes. If the design redefines
ownership or layering, also run `../why/SKILL.md` on the existing shape, so the rationale becomes a constraint, not a
guess. Keep the grounding as files the runners can read (for example under `/tmp/architect-<slug>/`).

Skip Phase A only when the work is greenfield with no surrounding system to integrate.

## Phase B: Sketch

Read `../arena/SKILL.md` and run it with the design-sketch task and the Phase A grounding paths. Each runner's brief
is the text of `references/runner-prompt.md` plus the task, the grounding paths and where to write; each candidate
produces a design package shaped per `references/rationale-template.md`, in its own worktree. Runners follow arena's
Phase A: one per model family by default, from `rlm.find_models`.

```python
SKILL_DIR = "..."  # this skill's directory, as Ultron listed it
runner_prompt = await read(f"{SKILL_DIR}/references/runner-prompt.md")
template = await read(f"{SKILL_DIR}/references/rationale-template.md")
TASK = (f"{runner_prompt}\n\n## Task\n...\n\n## Grounding\n<paths>\n\n## Rationale template\n{template}\n\n"
        "Write the sketch and DESIGN.md (the rationale) under design/ in your worktree.")
```

Design it twice. Require at least two structurally distinct candidates before synthesis, even when the first looks
sufficient: whole-shape alternatives, not point fixes inside one shape (the exhaust-the-design-space principle (`../principle-exhaust-the-design-space/SKILL.md`)). If the
runners converge, spawn one more with a brief that rules out the converged shape.

Screen every candidate against `references/design-red-flags.md` before synthesis. Assume the next contributor is an
agent that sees only the files it opened, copies the nearest example, and takes the shortest path that compiles.
Prefer the design where a change that looks right from one file is right for the whole repo.

Compare viable candidates on interface depth. Prefer the design that hides more complexity behind a smaller, simpler
public surface. A rich interface can keep call chains short by concentrating capability instead of scattering it
across layers.

Arena returns one synthesized design package; its synthesis decision fills the rationale's "Synthesis decision"
section.

## Phase C: Agree (opt-in)

Default: go straight to implementation with the synthesized design. No human checkpoint.

When the user asked for one ("architect with checkpoint", "show me before implementing"), present the synthesized
design in a plain reply and end the turn for sign-off.

The synthesis can ship as its own commit either way, the "scaffold first" mode of the foundational-thinking principle (`../principle-foundational-thinking/SKILL.md`).
Planned and scoped breakage during fill-in is fine, per the outcome-oriented-execution principle (`../principle-outcome-oriented-execution/SKILL.md`). For adversarial pressure
on the design before implementing, run `../interrogate/SKILL.md` on the synthesized sketch.

If the human pushes back on the shape (at a checkpoint or later), treat it as Phase A evidence: re-ground and re-run
Phase B before writing more code.

## Phase D: Implement against the sketch

Replace `not implemented` bodies with code, pseudocode with logic. The synthesized sketch is the contract.

Deviations from the sketch are signal to surface, not friction to absorb silently. Log each one
(`state.setdefault("deviations", []).append("parse() needs a clock: sketch missed time-dependence")`). If a function
needs a parameter the sketch did not anticipate, ask whether the sketch was wrong, the requirement was missed, or the
implementation is overreaching.

## Phase E: Scrap when the architecture is wrong

If implementation keeps producing friction the sketch cannot absorb, throw the sketch out. Do not bolt fixes onto a
wrong design, per the redesign-from-first-principles principle (`../principle-redesign-from-first-principles/SKILL.md`) and the fix-root-causes principle (`../principle-fix-root-causes/SKILL.md`).

The signal is a pattern, not single instances. Tells:

- The same shape of workaround appearing across unrelated code.
- Several unrelated edge cases that all need special-case branches.
- Types that need escape hatches (`any`, casts, optional fields always set in practice) to compile.
- The "we need a lock" reflex when the sketch said the state was not shared.
- Callers having to know the abstraction's internal rules to use it.
- Two or more independent Phase D deviations of the same shape.

Use judgment. A few edge cases do not condemn an architecture. Some problems are legitimately complex; complexity in
the data is not complexity in the design.

When you scrap:

1. Re-run `how` over what has been built.
2. Redesign as if the new constraints had been day-one assumptions.
3. Subtract before adding, per the subtract-before-you-add principle (`../principle-subtract-before-you-add/SKILL.md`). The new sketch is smaller than the old one before
   it grows.
4. Return to Phase B and re-run the arena.

## Outputs

The caller's usage written first and the type sketch derived from it. One file with new types and signatures for
small changes; a module map plus type definitions for larger work. The rationale ships alongside, shaped per
`references/rationale-template.md`, including the usage sketch and the synthesis decision.
