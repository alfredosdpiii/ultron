### Refactoring

**You own the contract. The structure changes; the behavior does not.**

If the cleanup reveals a missing feature or a real bug, split it out and ship the structural change first against the
pinned contract. A redesign is allowed, but name it and route to Feature. Large or cross-cutting structural work goes
to `../figure-it-out/SKILL.md`; this playbook is the focused-to-medium change.

1. Pin the behavior contract first. `../how/SKILL.md` over the subsystem, then a characterization test, snapshot or
   equivalence harness that captures current behavior before any structure moves. Type check and lint are not a pin.
2. Name the structure the code is missing (`the model-the-domain principle (`../principle-model-the-domain/SKILL.md`)`). Boring code stays when the shape is already
   clear and local. The reshape must delete branches or invalid states, not add indirection.
3. Name the target shape: module layout, types and call graph as if built today
   (`the foundational-thinking principle (`../principle-foundational-thinking/SKILL.md`)`, `the redesign-from-first-principles principle (`../principle-redesign-from-first-principles/SKILL.md`)`). If it crosses a function boundary,
   `../architect/SKILL.md` first.
4. Subtract before you add: dead code, one-caller wrappers, redundant validators, orphan references
   (`the subtract-before-you-add principle (`../principle-subtract-before-you-add/SKILL.md`)`). The smallest change that reaches the target ships
   (`the laziness-protocol principle (`../principle-laziness-protocol/SKILL.md`)`). A speculative cleanup gets reverted.
5. Move in small behavior-preserving steps, each keeping the pin green. For API reshapes, migrate every caller and
   delete the old API in the same wave (`the migrate-callers-then-delete-legacy-apis principle (`../principle-migrate-callers-then-delete-legacy-apis/SKILL.md`)`); no shims, no parallel
   paths. Spot-check every rename with `rg`: renames miss strings, prose and back-references. Delegate mechanical edits
   to a child (`worktree=True`, `model=state["models"]["code"]`) with file paths, the names moved and the behavior
   to hold. A rename across many files is a codemod you write and rerun (`the build-the-lever principle (`../principle-build-the-lever/SKILL.md`)`).
6. Prove behavior is unchanged on the real artifact (`the prove-it-works principle (`../principle-prove-it-works/SKILL.md`)`). For larger reshapes, an equivalence
   check: a script diffing old and new outputs, a recorded baseline replayed, or a smoke run on the real surface.
7. Confirm it is worth keeping: the measure is reduced reader load (`the minimize-reader-load principle (`../principle-minimize-reader-load/SKILL.md`)`). If the diff does
   not lower it somewhere, revert.
8. Rebase into ordered commits: subtraction, then reshape, then follow-on cleanup, each green
   (`the sequence-verifiable-units principle (`../principle-sequence-verifiable-units/SKILL.md`)`). Run `playbooks/opening-a-pr.md`.

**Reply:** the structure that changed, the pin, the equivalence proof, the reader-load delta, what shipped and what got
reverted. No new behavior.
