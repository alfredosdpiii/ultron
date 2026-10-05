### Orchestrate

**You own the program, never the code. Author briefs, drain completions, keep the frontier green, decide.** For a
project or queue that outlives any single agent: many PRs, many children, the human checking in rarely. One task
driven to a predicate is `playbooks/autonomous-run.md`; one ambitious run needing a bespoke workflow is
`../figure-it-out/SKILL.md`. Work one agent could finish inside the budget is not a program: do it directly.

Three rules carry the rest.

- Completions are queue events, not interrupts.
- Every spawn carries the standing orders verbatim.
- The brief is the product. A vague brief fails quietly, because a child cannot ask you a question.

Ceremony scales with the program. On cheap, near-identical units, collapse it as each section says.

#### Delivery shape

Pick one at framing and write it into the standing orders.

- **Program.** Workers build; the root lands verified units itself (fast-forward or clean cherry-pick, then push) or
  through a landing track. Workers never merge.
- **Full.** Independent PRs, landing authority granted. One owner per PR carries it from build to merge; nothing
  merges without the root's clean verdict.
- **Stack.** The operator wants review before landing, the work is sequenced or coupled, or merge authority is
  withheld. Owners build and verify; the root appends each verified PR to one linear base-branch stack the operator
  lands.

Items the operator names stay the operator's: they stop at merge-ready. When the operator asks for the plan or the
protocol, state it and stop; execution starts on an explicit go.

#### Roles

- **Root (you).** Frames, writes briefs, drains completions, owns the human report, makes judgment calls. Never
  authors or edits code: conflicted merges, restacks and code changes are always units. In Stack mode the root is the
  only topology writer.
- **Sub-coordinator.** One per track, only when the program exceeds what your drains can handle (roughly ten children
  in flight). `rlm.spawn(brief, name=f"track-{slug}", depth=2, worktree=True, model=state["models"]["judgment"])`
  (`depth=1` when its workers need no children of their own). It owns its track's units, writes its workers' briefs,
  keeps a rolling window of about ten children, and returns rollups at wave boundaries, never raw child reports.
  Nesting stops at three levels: root, track, worker.
- **Worker or owner.** `rlm.spawn(brief, name=..., worktree=True, model=state["models"]["code"], timeout_ms=...)`;
  `hard` for the gnarly units. Prefer fewer, broader workers. One writer per worktree or branch
  (`the separate-before-serializing-shared-state principle (`../principle-separate-before-serializing-shared-state/SKILL.md`)`). Set `timeout_ms` to at least the longest past run of that
  kind: a stuck child then ends on its own and arrives as an event instead of needing a liveness probe.
- **Verifier.** A child on a different model family from the unit's worker, or a swarm per `../swarm/SKILL.md`.

#### State

The tables live in `state["orch"]` (survives kernel restarts) and are mirrored at every drain to a repo-local notes
file kept out of version control, so a new session can resume. One writer per table: you.

```python
import json, pathlib

slug = "auth-migration"
notes = pathlib.Path(f".orchestrate/{slug}.json")
orch = state.setdefault("orch", {
    "standing": [],   # numbered standing orders, one constraint each
    "units": {},      # id -> {"track", "state", "branch", "pr", "head", "brief"}
    "ledger": {},     # f"{pr}@{head}" -> verdict
    "inbox": [],      # completion pointers awaiting a drain
    "gates": [],      # {"question", "options", "default"} for the human
    "frontier": [],   # bottom-to-top PR list, recomputed after every merge
})

def save():
    notes.parent.mkdir(exist_ok=True)
    notes.write_text(json.dumps(orch, indent=1))

def status():
    counts = {}
    for unit in orch["units"].values():
        counts[unit["state"]] = counts.get(unit["state"], 0) + 1
    print(counts, "| gates:", len(orch["gates"]), "| frontier:", orch["frontier"][:3])

await bash('''grep -qx '.orchestrate/' .git/info/exclude || echo '.orchestrate/' >> .git/info/exclude''')
save()
```

Standing orders are the register of constraints (model policy, delivery shape, verification bar, forbidden paths,
escalation policy). When you catch yourself restating an instruction, append it before you act
(`the encode-lessons-in-structure principle (`../principle-encode-lessons-in-structure/SKILL.md`)`). Keep the decision trail per `../show-me-your-work/SKILL.md`.

#### The brief

A field you cannot fill is a unit you have not scoped.

```
GOAL         one sentence, the outcome, executable by a stranger with no chat access
SCOPE        paths this unit may write; paths it may not; its branch
CONTEXT      file and PR pointers; upstream reports pasted in full when this unit depends on them
ACCEPTANCE   checkable criteria, one per line
VERIFY       exact commands or the surface to drive, plus known gotchas
TIMEBOX      rough cap; on expiry, return partial findings and stop
FORBIDDEN    no rebase or force-push of others' branches, no fixes outside scope, plus unit-specific bans
REPORT       end with rlm.finish: status, branch, head SHA, PR, verdict, what you ran, deviations, follow-ups
STANDING     the standing orders, verbatim
SKILL        read <absolute path of the rigor skill>/SKILL.md in full and work by it
```

A one-command unit gets the template collapsed to a paragraph that still names goal, scope, the verify command and the
report shape. A sub-coordinator brief adds its track boundary and unit list, its spawn budget, the drain protocol and
the rollup format (per child: name, status, PR, head, verdict, one line; then track status and frontier delta). A
dependency is a context relay: paste the upstream report into the downstream brief. Audit one sampled worker brief per
sub-coordinator per wave, alongside the wave; a failing brief stops that track's next refill and fixes the
sub-coordinator's instructions. Never resume-chain a brief; respawn fresh with consolidated scope.

#### Steps

1. **Frame.** A countable done predicate ("all 126 units merged, each ledger-verified `unit-test-verified` or better"),
   the units, rough effort, delivery shape and budget. If one agent could finish it, stop and do it directly. By
   about 70% of the budget, stop spawning and land what is verified. Contested decompositions go through
   `../arena/SKILL.md` first. Present the framing once; reversible prep proceeds.
2. **Install.** Write the standing orders, seed `state["orch"]` and the frontier from existing PRs
   (`gh pr list --json number,headRefName,baseRefName,headRefOid`), open the trail.
3. **Pilot.** Push one unit through the whole path (brief, worker, verification, landing, ledger row) to falsify the
   brief template, the verify recipe and the unit size while that costs one child. Fix the contract from its evidence.
   On near-identical cheap units, the first unit is the pilot and fan-out starts when it lands.
4. **Scale.** A rolling window up to the in-flight cap, refilled as children finish, never blocking batches. Spawn
   sub-coordinators only past the one-drain threshold. Relay upstream reports into downstream briefs; sibling
   communication goes upward only.
5. **Drain.** On each `child_done` event, append a pointer to `orch["inbox"]` and return to what you were doing; never
   review a diff inside a drain. Drain in batches at the end of a critical section (writing a brief, a stack operation,
   a conflict decision, a gate, a ledger or frontier update), at a track rollup, and before a human report:
   `results = await rlm.collect(handles)` for the finished ones, classify each (landed, needs-verify, failed, noise),
   update units and ledger, `save()`, `status()`, then spawn the next wave in one cell. Account for every spawned
   child at its rollup: arrived, respawned, or its scope explicitly absorbed. A drain turn ends with three lines:
   counts by state, what changed, gates open.
6. **Verify.** Scale it to the unit. When VERIFY is one cheap command, the worker runs it and reports output; trust
   `check.outcome == "verified"` and spot-check the rest. A dedicated verifier is for expensive, judgment-laden or
   high-blast-radius units. Ledger verdicts, keyed by PR and head SHA: `live-verified`, `unit-test-verified`,
   `type-check-only`, `verifier-blocked`, `verifier-failed`. CI green is an input, not a verdict; behavioral work needs
   better than `type-check-only`; `verifier-failed` gets a fix unit; a new head SHA voids the row unless the patch-id
   rule in `playbooks/shipping.md` keeps it. A unit is done when its output is pushed and its row written.
7. **Land.** Continuous from the first verified unit, never a terminal phase. Program: the root lands verified units
   bottom-up per `playbooks/shipping.md`. Full and Stack: per **Owners** below. Recompute the frontier from `gh`
   after every merge and stack change.
8. **Close.** Drain the last completions, reconcile every spawned child to a terminal row (done, abandoned,
   reconciled), confirm the predicate on the real artifact, confirm every landed PR has a verdict for its head SHA,
   fold recurring corrections into the standing orders or the brief template. Leave the notes file; it is the
   postmortem.

#### Owners (Full and Stack)

One owner child per PR (`depth=1` so it can run its own verifiers) carries: build, a first push and a PR opened ready
within its first unit of work, a decision trail, self-proof on the real artifact (`the prove-it-works principle (`../principle-prove-it-works/SKILL.md`)`),
review-bot triage per `playbooks/babysit.md`, the pre-PR steps in `playbooks/opening-a-pr.md`, one rebase onto
current trunk before reporting code-ready, then the babysit loop to green. It reports the code-ready head SHA once the
code is final, and every later head that changes the patch. Owners push only their own branches
(`git push --force-with-lease` after a `git ls-remote` check); never a shared one. Self-contained PRs branch from main
and run in parallel; only overlapping work serializes.

**Verify each round.** At each code-ready head, fan out a swarm (`../swarm/SKILL.md`) and aggregate one verdict. Lanes:
re-run the gates at that SHA; prove the load-bearing behavior live on the real surface; two or more diff-audit lanes,
each with one focus (consumer parity with trunk, lifetimes and races, data and config safety), distrusting the PR body;
and a regression lane running the same scenario on trunk (if trunk lacks the feature, record that and gate the added
behavior and the end state the user waits for). The live lane is the floor; without it the verdict is not clean.
Every proven finding goes back to the owner in one fix round, each behavior finding with a red test covering every
site with the same defect.

**Full.** On a clean verdict the owner merges, only from a head freshly rebased onto trunk: right before the merge,
`git fetch`, check `git merge-tree --write-tree HEAD origin/main` is clean and that no path changed on trunk since the
merge base is one the PR touches or one that decides its CI; otherwise rebase, report the new head, wait for CI. The
patch-id rule decides whether the verdict still holds. Then a fresh owner takes the next item.

**Stack.** No owner merges, arms auto-merge or closes. On a clean verdict the root appends the PR: fetch the parent,
rebase the child onto the exact parent tip, push with `--force-with-lease`, set the PR base to the parent branch. Only
the root PR targets trunk. When trunk drifts, the root rebases the chain bottom-up; an owner fixes conflicts in its own
slice; verdicts at rewritten SHAs go through the patch-id rule and re-verify when they no longer hold. Deliver the
chain with each link's verdict on its PR.

A new raise of a pinned gate or budget value needs your fresh countersign after verifier proof; absorbing values
already on main is drift, not a raise. You never give or bypass an approval the forge enforces.

#### Liveness and failure

- Never resume a child to check on it, and never read its files or logs for progress. Results and timeouts arrive as
  events. Between drains, judge progress only by side effects: pushes, PR and check changes, ledger rows.
- Retry by failure mode: out of budget or memory, respawn with smaller scope; network drop, retry as-is; tool error,
  retry on a different model; unknown, once. Two retries, then abandon the unit and replan around it.
- A child that returns late reconciles against the current frontier and ledger before anything is accepted. Salvage
  unique findings through a fresh unit, never a blind merge.
- When spawning would produce garbage tree-wide (bad upstream output, broken acceptance, dead infra), put a stop line
  at the top of the standing orders, let in-flight work finish, fix the cause, clear it.
- Bound your own retries too. After a few consecutive tool failures, write a terminal handoff to the notes file (what
  is done, where it lives, the exact resume step) and end the run.
- On the operator's stop, every child gets a zero-writes hold at once.
- After a restart, re-read the notes file, recompute the frontier from `gh`, reattach work by PR and branch, respawn a
  sub-coordinator per track from its stored brief and current state, drain, resume.

#### Escalation

Reaches the human, batched into the status report: irreversible actions (force-push to shared branches, deploys,
deletions, closing someone else's PR), product or preference calls no experiment settles, a standing order that
contradicts reality, a dead end that survived a replan. Park each in `orch["gates"]` with a default and route work
around it. Never reaches the human: restack mechanics, retries, CI flake triage, review-thread triage, format fixes,
and "should I keep going". Mid-run discoveries fix only what blocks the frontier; everything else becomes a follow-up.

**Reply:** at checkpoints and close: the predicate and the count against it from the tables, tracks and what each
landed, the frontier (PRs and SHAs), the verdict summary, what was abandoned and why, gates awaiting the human (the only
asks), and the notes and trail paths, with PR links. Numbers from the tables, not narrative.
