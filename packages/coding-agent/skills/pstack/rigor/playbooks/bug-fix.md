### Bug fix

**You own this task. Plan, review, verify.** Delegate the fix to a child, stay in the lead.

Be scientific. Every shipped line traces to runtime evidence. Belt-and-suspenders that "might help" is a hypothesis,
not a fix, and does not ship. When evidence refutes a hypothesis, revert what it motivated. The smallest change the
evidence justifies ships.

1. Reproduce it yourself on the matching surface (tmux for CLIs and TUIs, a browser MCP for web UIs), even when a
   debug protocol says to ask the user to reproduce. Ask the user only with a stated, specific reason the surface
   cannot be reached, and only after driving it as far as it goes. If it won't reproduce, synthesize the trigger,
   tighten conditions, or instrument until it fires.
2. Binary-search the cause. Form candidate hypotheses and rule them out until one survives. Seed them with
   `../how/SKILL.md` over the subsystem and `../why/SKILL.md` for regression history. Each pass, take the split that
   cuts the most remaining space, get runtime evidence, eliminate. When state is unclear, add logging and read it as
   the code runs. Don't guess. Keep the hypothesis table in `state["hypotheses"]`; for a long unattended hunt, see
   `playbooks/autonomous-run.md`. Confirm the surviving mechanism with runtime evidence before step 3.
3. Plan the fix. If it crosses a function boundary, `../architect/SKILL.md` first. Delegate the implementation with a
   specific scope: `rlm.spawn(brief, name=..., worktree=True, model=state["models"]["code"])` (`hard` for subtle
   causes).
4. Verify on the same surface. The original repro now passes. Inconclusive or wrong-surface is not a pass; flag it.
   Unit tests show branch behavior, not bug absence.
5. Stage commits so the failing repro lands before the fix (`the sequence-verifiable-units principle (`../principle-sequence-verifiable-units/SKILL.md`)`). Use
   `../tdd/SKILL.md` when the bug has a cheap local test path; skip it when the test would be expensive,
   integration-heavy or unclear.
6. Run `playbooks/opening-a-pr.md`.

**Reply:** what was broken, root cause, fix, how you verified. Paste failing-then-passing repro output verbatim.
