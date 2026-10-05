### Autonomous run

**You own the exit condition. Define done, then drive to it without stopping.** In Ultron a run that outlives your
turns is the session goal: the user sets `/goal <objective>` and `/goal check <command>`, and a background job works it
in its own REPL until `goal.complete` passes. Only the user can set or change a goal.

1. State the exit condition as a checkable predicate before the first iteration (tests green, repro fixed, all N PRs
   merged, pixel diff zero), and the shell command that exits 0 exactly when it holds.
2. Check for a goal: `g = await goal.get()`.
   - No goal, or one for different work: do not start the loop yourself. Propose the exact lines in your reply and end
     the turn, for example:

     ```
     /goal Fix the flaky reconnect in src/net/client.ts so the reconnect suite passes 20 runs in a row. Work by the rigor skill (<absolute path of this skill>/SKILL.md), playbook bug-fix, and keep a decision trail.
     /goal check npm run test -- test/reconnect.test.ts --repeat 20
     ```

     Name the rigor skill's path in the objective, since the goal job does not see your chat. A predicate no command
     can check gets no check line; say that its completion will be recorded as unverified.
   - You are the goal job (your brief names a goal revision): work it as below.
3. Each iteration makes the smallest change the evidence justifies, then `r = await goal.check()`. Commit when it moved
   the predicate; discard changes that didn't help. Belt-and-suspenders that "might help" is reverted
   (`the sequence-verifiable-units principle (`../principle-sequence-verifiable-units/SKILL.md`)`). Keep what you tried in `state["goal"]`. Ten identical failing checks pause
   the goal, so change approach when a check keeps failing the same way. Independent attempts can run as children
   (`worktree=True`), merging the one that works.
4. Mid-run discoveries are yours: broken skills, related bugs, flaky verifiers, tooling failures, fixable drift. Fix
   them in their own change and return to the predicate. Do not park reversible work for the human. Surface only
   irreversible actions, product or preference calls no experiment settles, or a real dead end.
5. Checkpoint every iteration via `../show-me-your-work/SKILL.md`: a row for what changed and whether the predicate
   moved.
6. Stop only when the predicate holds: `await goal.complete(revision, summary, evidence=[...])`, with the commands run
   and their outcomes. A plateau is not a stop; pivot and push past it. Never relax the predicate. A genuine external
   block after real attempts: `await goal.blocked(revision, reason)`. Re-read `goal.get()` before ending, since the
   user may have edited the goal (new revision).

**Reply:** the exit condition, iterations run, what landed, what was discarded, the final check result. When no goal
was set, the proposed `/goal` and `/goal check` lines.
