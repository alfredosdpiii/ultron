### Perf issue

**You own the measurement story. Plan, review, verify the numbers.** Tie every fix to a measurement; don't read source
instead of measuring.

1. Capture a baseline trace on the matching surface. Vet it, and each later number, with
   `../benchmark-checklist/SKILL.md`.
2. `../how/SKILL.md` to ground hypotheses. Don't claim a ceiling without running it. Try the performance mantras in
   order, cheapest first, and stop when one meets the target:
   1. Don't do it. Stop work whose result nothing uses.
   2. Do it, but don't do it again.
   3. Do it less.
   4. Do it later.
   5. Do it when they're not looking.
   6. Do it concurrently.
   7. Do it cheaper.
3. Plan the fix from the trace. If it crosses a function boundary, `../architect/SKILL.md` first. Delegate the
   implementation to a child (`worktree=True`, `model=state["models"]["code"]`). Review the diff. Capture a post-fix
   trace. Verify each attempt before the next (`the sequence-verifiable-units principle (`../principle-sequence-verifiable-units/SKILL.md`)`).
4. Parse and compare the artifacts in the REPL (load JSON into sqlite or pandas, diff). Inconclusive or wrong-surface
   is not a pass; flag it.
5. Cite the measurement in the PR.
6. Run `playbooks/opening-a-pr.md`.

For sustained improvement against a metric, use `playbooks/hillclimb.md`.

**Reply:** baseline number, post-fix number, delta, artifact path.
