### Hillclimb

**You own the metric and the experiment's integrity. Supervise and review; delegate the attempts.** For sustained,
iterative improvement of one measurable thing against a target. A one-off fix is Bug fix or Perf issue.

Core discipline: one change, one measurement, keep or revert. Never stack untested changes, never claim a win from
code inspection (`the prove-it-works principle (`../principle-prove-it-works/SKILL.md`)`).

1. Ground the workload before choosing the metric. Run `../how/SKILL.md` over the target, name the workload
   dimensions that move the result (data size, history, state, concurrency), and pick a case that reproduces the
   user's complaint. If none does, fix the repro instead. Then fix one metric, its better direction, and a checkable
   stop predicate that pairs a target with a floor on attempts ("at least 50% better than baseline and at least 10
   iterations"). Use the user's numbers when given.
2. Build the measurement harness, prove its sensitivity, freeze it (`the build-the-lever principle (`../principle-build-the-lever/SKILL.md`)`). Contrasting
   workloads must separate as expected; if they don't, revise the workload or metric. Vet it with
   `../benchmark-checklist/SKILL.md`, and make it print its error count and a count of the work done. Frozen, one
   command emits the metric as a median of N runs. Record the baseline and a green run of the regression gate before
   any change.
3. Open the decision log via `../show-me-your-work/SKILL.md`: a `decision.tsv` kept out of the tree, one row per
   attempt (id, hypothesis, change, before, after, delta, tests, verdict, note). Read it before each attempt.
4. Ground each hypothesis in a named mechanism ("defer X off the boot path because it blocks first paint"), not "try
   memoizing something". For perf metrics, order hypotheses by the mantras in `playbooks/perf-issue.md` step 2
   (their order, not their stop rule).
5. Loop, one hypothesis per iteration:
   - A child makes the change with a tight scope (`worktree=True`, `model=state["models"]["code"]`). Review its diff
     instead of typing it. Independent live hypotheses fan out to parallel children, each in its own worktree
     (`the separate-before-serializing-shared-state principle (`../principle-separate-before-serializing-shared-state/SKILL.md`)`).
   - Measure before and after with the frozen harness; run the regression gate.
   - Accept only when the metric moves past noise and the gate stays green. Otherwise revert in full.
   - One commit per accepted fix, staging only the files changed (`git add <files>`). Log the row either way.

   Each iteration ends in a check before the next (`the sequence-verifiable-units principle (`../principle-sequence-verifiable-units/SKILL.md`)`). For an unattended run,
   drive it with `/goal` (`playbooks/autonomous-run.md`), with this playbook's stop rule.
6. Push past the first plateau. After several rejects, pivot category, combine near-misses, re-read the source, try
   something more radical. Correctness and simplicity outrank the number: revert a win that breaks behavior, keep a
   simplification that holds it (`the laziness-protocol principle (`../principle-laziness-protocol/SKILL.md`)`).
7. Stop when the predicate is met, or the remaining ideas are marginal. Don't relax the predicate. Don't quit while
   cheap untried hypotheses remain. Stuck, surface it instead of spinning.
8. Run `playbooks/opening-a-pr.md` with the accepted commits stacked in landing order.

**Reply:** metric and target, baseline to final with percent delta, iterations (kept vs reverted), each accepted fix on
one line, the `decision.tsv` path, and the best next idea.
