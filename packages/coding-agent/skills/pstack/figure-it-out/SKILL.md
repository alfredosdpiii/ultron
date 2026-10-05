---
name: figure-it-out
description: "Design an auditable playbook when no narrower one fits: a large migration, an ambitious multi-part change, or work a human reviews after stepping away. Scales rigor to the task, runs a hypothesis loop, and logs decisions via show-me-your-work. Use for /skill:figure-it-out, 'figure it out', a large migration, or when no narrower playbook applies."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/figure-it-out
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Figure it out

When the task matches no playbook, design one. The deliverable before any code is the workflow itself: a sequence of
phases that scales rigor to the task, runs the scientific method, and leaves a decision trail a human can audit after
stepping away.

## Start

Read the Principles section of `../rigor/SKILL.md` first. Then track the phases in the REPL; `state` survives kernel
restarts, so the plan does too.

```python
state["plan"] = [
    {"step": "read rigor principles", "done": False},
    {"step": "A frame", "done": False},
    {"step": "B design the workflow", "done": False},
    {"step": "C run the loop", "done": False},
    {"step": "D audit trail", "done": False},
    {"step": "E verify and hand back", "done": False},
]
print(" | ".join(("[x] " if s["done"] else "[ ] ") + s["step"] for s in state["plan"]))
```

## Phase A: Frame

Ground first, then commit. Do not start the run until you can state:

- The definition of done as a falsifiable predicate (the prove-it-works principle (`../principle-prove-it-works/SKILL.md`)), ideally as one command that exits 0
  only when it holds.
- Scope, quantified: rough units and effort, plus the blockers grounding surfaced.
- The rigor level, biased high. One-way doors and high blast radius get more; reversible low-stakes steps get less.
  Rigor is gates and artifacts, not "try harder".

Present the framing and tradeoffs before committing to a long run. Reversible work proceeds
(the never-block-on-the-human principle (`../principle-never-block-on-the-human/SKILL.md`)), but a multi-hour run earns one checkpoint: reply with the framing and end the
turn.

For a run that should keep going until the predicate holds, suggest the lines for the user to send; only the user
sets a goal:

```
/goal <the objective, in one sentence>
/goal check <the predicate command>
```

A background job then works the goal in its own REPL and can end it only with `await goal.complete(revision, summary,
evidence=[...])`, which runs the check. Without a goal, run the phases yourself in this session.

## Phase B: Design the workflow

Decompose into atomic, independently landable units. Sequence riskiest unknown first. Scaffold and verification come
before features (the foundational-thinking principle (`../principle-foundational-thinking/SKILL.md`)).

- Build the verification harness before the work, with the baseline captured from the pre-change state, so the check
  reads as "old value vs new value" (`state["baseline"] = ...`).
- For one-way-door design decisions, run `../architect/SKILL.md` (it runs `../arena/SKILL.md`). Skip it for mechanical
  work whose shape is already concrete. A second arena over a settled design is over-engineering
  (the laziness-protocol principle (`../principle-laziness-protocol/SKILL.md`)).
- Decide what fans out. Parallelize only across seams, each worker in its own worktree
  (`rlm.spawn(..., worktree=True)`, then `rlm.merge`), per the separate-before-serializing-shared-state principle (`../principle-separate-before-serializing-shared-state/SKILL.md`); see
  `../swarm/SKILL.md`. Do not over-fan, and never spawn to read or classify files: narrow with code and `rlm.map`.
- Write the designed phase list down. That list is what the human reviews.

Then execute the design. Insert its steps into `state["plan"]` as concrete items after "C run the loop" and before
"D audit trail". Run each under the Phase C discipline, and add the Phase D log entry as each step lands rather than
saving the whole trail for the end.

## Phase C: Run the loop

Each unit is an experiment. State the hypothesis, make the smallest change, measure against the predicate on the real
artifact, keep it if it advanced, revert it if it did not. Verify each unit before starting the next instead of
batching checks at the end (the sequence-verifiable-units principle (`../principle-sequence-verifiable-units/SKILL.md`)).

- Verify by inspecting the artifact, never a self-report. When something passes too easily, suspect the observation
  method before the system.
- Pair delegated work with a judge. A child's verdict counts only when `result["check"]["outcome"] == "verified"`,
  and even then rerun its key evidence yourself. If a worker games the gate, reset and harden the contract. If the gate
  itself is wrong, fix the gate in its own change rather than routing around it.
- A verdict is VERIFIED, NOT VERIFIED, or INCONCLUSIVE. Inconclusive is not a pass. Do not hide a negative.

## Phase D: Keep the audit trail

Log the run via `../show-me-your-work/SKILL.md`. figure-it-out's work is usually ambitious enough to commit the trail
so the reviewer can read it in the PR. The trail plus the diff is what lets the human come back and trust the work.

## Phase E: Verify and hand back

Check the whole against the Phase A predicate on the real product, not just the harness. Encode any recurring
correction as a gate, a lint rule, a check, or a script (the encode-lessons-in-structure principle (`../principle-encode-lessons-in-structure/SKILL.md`)).

**Reply:** the playbook you designed, the rigor level and why, the decision-trail path, what is verified against the
predicate, and what is still open.
