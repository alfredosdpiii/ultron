### Worktree cleanup

**You own the disk and the safety gate.** Prune merged or abandoned git worktrees to reclaim space. Deletion is
irreversible, so every step guards against deleting something in use or holding uncommitted work. This is the one
playbook that deletes user state with no code review, so the gates are the review.

1. Snapshot and audit. Record `df -h .`. Ultron's own child worktrees come first: `await rlm.worktrees.list(all=True)`
   shows them, and `await rlm.worktrees.cleanup()` removes finished children's worktrees and those of crashed sessions,
   keeping branches with unmerged work. For the rest, classify every worktree from `git worktree list --porcelain`,
   never hand-typed paths (`the encode-lessons-in-structure principle (`../principle-encode-lessons-in-structure/SKILL.md`)`):

```python
import json, re, time

await bash('''git fetch origin --quiet''')
trunk = "origin/main"
out = await bash('''gh pr list --author "@me" --state all --limit 500 --json number,state,headRefName''')
prs = json.loads(out) if out.ok else []
by_branch = {p["headRefName"]: f"#{p['number']}/{p['state']}" for p in prs}
paths = re.findall(r"^worktree (.+)$", await bash('''git worktree list --porcelain'''), re.M)[1:]
rows = []
for wt in paths:
    size = (await bash(f'''du -sh "{wt}" | cut -f1''')).strip()
    head = (await bash(f'''git -C "{wt}" rev-parse HEAD''')).strip()
    stamp = await bash(f'''git -C "{wt}" log -1 --format=%ct''')
    age = (time.time() - int(stamp.strip())) / 86400 if stamp.ok else -1
    merged = (await bash(f'''git merge-base --is-ancestor {head} {trunk}''')).ok
    status = (await bash(f'''git -C "{wt}" status --porcelain''')).splitlines()
    wip = sum(1 for s in status if not s.startswith("??"))
    branch = (await bash(f'''git -C "{wt}" symbolic-ref --quiet --short HEAD''')).strip()
    pr = by_branch.get(branch, "-")
    bucket = "hold-wip" if wip else "hold-open-pr" if "OPEN" in pr else "safe" if merged or pr != "-" else "review"
    rows.append({"path": wt, "size": size, "age_days": round(age), "merged": merged, "wip": wip,
                 "scratch": len(status) - wip, "branch": branch, "pr": pr, "bucket": bucket})
state["worktree_audit"] = rows
for r in rows:
    print(r["bucket"], r["size"], f"{r['age_days']}d", r["pr"], f"wip:{r['wip']}", r["path"])
```

   Squash-merged branches are not ancestors of trunk, so PR state is the stronger signal.
2. The bucket is advice, not permission. Ask which worktrees the user or a running session is using, and cross-check
   every candidate against that set; the in-use set wins over `safe`.
3. Verify usage before deleting. For anything you doubt, look for live users: processes with the path as cwd
   (`lsof +D <path>` or `fuser`), a tmux pane in it, a recent session that worked there (`rg -l <path>` over this
   project's sessions under the agent dir `sessions/`).
4. Pause on irreversible loss. `wip:N` is N tracked uncommitted edits: show the diff and get a decision first. A clean
   worktree is recoverable from its branch; uncommitted work is not. Untracked scratch is throwaway, but name the
   files. Clean, merged and not in use proceeds; wip and in-use pause.
5. Prune the confirmed set. Per path, `git worktree remove --force <path>`; if the directory survives on ignored build
   artifacts, `rm -rf` it, then `git worktree prune`. Branch refs survive, so no commits are lost. Confirm with
   `df -h .` and a fresh `git worktree list`.
6. Other reclaimers when needed: package caches (npm, pnpm, uv, pip), build outputs. Clear only caches the user has not
   said to keep.

**Reply:** `df -h` before and after with the space reclaimed, the worktrees pruned, and a one-line reason for each held
back (in use by what, or uncommitted work).
