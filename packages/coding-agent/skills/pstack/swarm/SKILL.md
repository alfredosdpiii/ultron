---
name: swarm
description: "Fan out N parallel subagents over slices of a task or as a race on one brief, collect their checked verdicts, and return one report. Use for /skill:swarm, 'swarm this', parallel coverage, races, gauntlets and exploration; not for reading or classifying documents (that is rlm.map)."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/swarm
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Swarm

Fan out N parallel subagents. They may cover separate slices, race the same brief, or mix both. The parent waits,
aggregates their verdicts, and returns one report.

A swarm is for independent multi-step work: run, build, measure, fix, verify. It is not for reading, searching or
classifying files or documents. Narrow those with code (`await bash("rg ...")`, `h = await rlm.load(path)`,
`h.search(...)`) and judge what is left with `await rlm.map(task, chunks, contract=...)`. Each subagent re-sends its
prompt and transcript every turn; a frame costs one request.

Track the phases in the REPL before launching anything:

```python
state["plan"] = ["frame", "fan out", "aggregate", "report"]
```

## Phase A: Frame

1. State the done predicate and the artifact or report the swarm must return.
2. Choose the shape: partition into slices, race N workers on identical briefs, or mix both. For a race or mixed shape,
   declare `first pass`, `rank all` or `best-of` before spawning, and record it (`state["race_rule"] = "best-of"`).
3. Set N from the user or derive it from the shape. N is total workers.
4. Pick the worker model. Use the one the user names; otherwise omit `model=` so workers run on the settings default.
   For a model race, find each arm's model up front and name it in the report.

```python
ms = await rlm.find_models("grok")  # the model the user named (example query)
model = f"{ms[0]['provider']}/{ms[0]['id']}" if ms else None
```

5. Workers that write get `worktree=True`, so each has its own branch and no two clobber each other. Read-only
   workers share your tree and their brief says read-only. When workers verify or measure commits, each brief names
   the exact SHAs; a measurement brief also names the method (sample count, what one sample is, order). The worker
   records both in its result.

## Phase B: Fan out

Every brief stands alone: the goal, scope, exact slice or race arm, how to verify, and what to report. It ends with
the finish contract, so the result comes back as a checked verdict, not prose.

```python
FINISH = """
When done, call once: await rlm.finish(status, summary, evidence=[...], outputs={...}, changed_files=[...])
- status "passed" when you completed the slice (whatever you found), "blocked" when something outside you stopped
  it, "failed" when it could not be done.
- outputs: {"verdict": "PASS" | "ISSUES" | "BLOCKED", "issues": ["<file:line or command>: <one-line issue>", ...],
  "shas": [...], "method": "..."}. With proof of a defect, report ISSUES and list every issue you can prove, not only
  the first.
- evidence: commands with their outcome ("npm test: exit 0, 42 passed") or files with lines.
- changed_files: every file you created, edited or deleted ([] when read-only).
Then reply in two lines."""

slices = [{"name": "auth", "brief": "..."}, {"name": "billing", "brief": "..."}]  # or N copies of one brief
writes = False  # True when workers edit files
hs = await asyncio.gather(*[
    rlm.spawn(s["brief"] + FINISH, name=f"swarm-{s['name']}", model=model, worktree=writes) for s in slices
])
```

Spawn all N at once. Do only work no worker owns while they run; never check on their files or logs.

## Phase C: Aggregate

A slice counts only with a result whose host check is `verified` and whose outputs carry the SHAs and method its
brief named. Anything else (failed run, `contradicted`, `invalid` or `unchecked` check, missing fields) is a gap:
respawn that worker once with the same brief; after a second miss, record the gap. A gap is never a pass. For
coverage, every required slice needs a result. For a race, apply the rule declared in Phase A.

```python
def judge(s, item):
    r = item["result"]
    v = r.get("verdict") or {}
    out = v.get("outputs") or {}
    if r.get("status") != "succeeded" or (r.get("check") or {}).get("outcome") != "verified":
        return None
    if out.get("verdict") not in ("PASS", "ISSUES", "BLOCKED"):
        return None
    return {"slice": s["name"], "verdict": out["verdict"], "issues": out.get("issues", []),
            "evidence": v.get("evidence", [])[:2], "branch": (r.get("worktree") or {}).get("branch")}

rows, gaps, retry = {}, [], []
for s, item in zip(slices, await rlm.collect(hs)):
    row = judge(s, item)
    if row:
        rows[s["name"]] = row
    else:
        retry.append(s)
if retry:
    again = await asyncio.gather(*[
        rlm.spawn(s["brief"] + FINISH, name=f"swarm-{s['name']}-2", model=model, worktree=writes) for s in retry
    ])
    for s, item in zip(retry, await rlm.collect(again)):
        row = judge(s, item)
        if row:
            rows[s["name"]] = row
        else:
            gaps.append(s["name"])
state["swarm"] = {"rows": rows, "gaps": gaps}
for row in rows.values():
    print(f"{row['slice']:<16} {row['verdict']:<8} {len(row['issues'])} issue(s)")
```

Spot-check the evidence of PASS rows that matter: rerun a cited command, read a cited line. When workers wrote, bring
their branches in once the verdicts hold, and review the merged diff before committing:

```python
merged = await rlm.merge([h for h in hs], on_conflict="stop")
print(merged["ok"], [(x["name"], x["status"]) for x in merged["results"]])
```

For a race, merge only the winner, then discard exactly the losers, using the `worktree` info from their collect
results (run from the repo root you work in):

```python
results = await rlm.collect(hs)  # already finished: returns at once
winner = hs[0]  # your pick by the race rule
await rlm.merge([winner])
for h, item in zip(hs, results):
    wt = item["result"].get("worktree") or {}
    if h is not winner and wt.get("path") and not wt.get("removed"):
        print(await bash(f"git worktree remove --force {wt['path']} && git branch -D {wt['branch']}"))
```

Do not paste raw worker replies.

## Phase D: Report

One reply: the result table (slice or arm, verdict, issue count), one-line evidenced issues, gaps and dropouts, the
race rule when used, and what was merged.
