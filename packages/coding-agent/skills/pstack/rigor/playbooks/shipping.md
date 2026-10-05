### Shipping

**You own what lands. Verify each PR independently, land only the verified run from the root, then keep your hands off
the queue.** The half after `playbooks/babysit.md`. The forge is GitHub through `gh`.

1. **Verify every PR independently.** One child per PR, not batched, on the `judgment` model, spawned with a brief
   that says it did not write the code, checks out parent and head in its own worktree (`worktree=True`), and exercises
   the real surface (tmux for CLIs and TUIs, a browser MCP for web UIs) on both. It ends with
   `await rlm.finish(status, summary, evidence=[...], outputs={"verdict": "PASS" | "PASS+NOTES" | "FAIL", "head": sha})`
   and posts the verdict on its PR (`gh pr comment <pr> --body-file <file>`). Safe means a verdict from an agent that
   did not write the code. CI green is not a verdict; an approving bot review is not a verdict.
2. **Land only the contiguous verified run from the bottom.** Walk up from the lowest unmerged PR; stop at the first
   without `PASS` or `PASS+NOTES`. A verified PR above an unverified one is not landable. Report the ceiling as a PR
   number and what breaks the chain.
3. **Re-check that each verdict still describes the patch.** Record each verdict's head SHA, base SHA and
   `git diff <base> <head> | git patch-id --stable`. A rebase or retarget rewrites SHAs and can void a verdict without
   touching a check. Before landing, compare the recorded patch-id with the current one. Same patch: keep the code
   verdict, re-run mergeability and CI at the current head. Different only in tests, docs or lint config: build twice
   at the verdict SHA and once at the head; a difference that also shows between the two verdict-SHA builds, or is an
   embedded commit SHA, is noise. Judge each difference, report each kind of noise with its files. Anything else
   changed: re-verify. Never accept matching commit messages or an older SHA's green check.
4. **Prepare only the bottom PR.** Fetch trunk. Rebase the lowest verified branch onto the exact trunk tip when needed,
   push it, retarget only that PR (`gh pr edit <pr> --base <trunk>`), and re-run step 3. Leave descendants alone.
5. **Land one PR at a time.** Mergeable now: `gh pr merge <pr> --squash`. Requirements still running and the user
   asked for merge-when-ready: arm only that PR (`gh pr merge <pr> --squash --auto`). `autoMergeRequest` says only that
   auto-merge was requested for that one PR, never that the stack is safe.
6. **Watch the frontier without polling.** Start `gh pr checks <pr> --watch` as a job (`yield_after=0`); when its event
   arrives, read `gh pr view <pr> --json state,mergedAt,mergeStateStatus,statusCheckRollup,autoMergeRequest`. Merged
   when `mergedAt` is set or `state` is `MERGED`. Hard-fail only on `CLOSED` without `mergedAt`, a required check
   ending `FAILURE` or `CANCELLED` once auto-merge is no longer pending, or `UNSTABLE`/`DIRTY` with no auto-merge
   pending. `BLOCKED` while checks run or auto-merge is armed is not failure. If the queue stalls, diagnose before
   mutating anything.
7. **Recompute after every merge.** Fetch trunk, confirm the merged SHA is there, drop the PR from the frozen
   bottom-to-top list, inspect the new bottom PR's base, head, checks and patch-id. GitHub may retarget a child; do not
   assume it did. Repeat steps 3 to 6 for that one PR.
8. **Stop at the ceiling.** Report what landed, the next unverified PR, and what verifying it would take. Extending
   the run is a new pass through step 1.

**Reply:** the verified run and its ceiling, each PR's verdict and who produced it, what you armed and how you confirmed
it, what landed, and what the next gap needs.
