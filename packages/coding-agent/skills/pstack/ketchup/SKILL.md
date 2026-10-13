---
name: ketchup
description: "Catch the user up on everything in this session since their last message, in plain words. Give each action item an executive summary with enough context to make the call. Use for /skill:ketchup."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/ketchup
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Ketchup

Catch the user up on everything in this session since the last message they wrote before `/skill:ketchup`. Runtime
events, child results and messages from other agents are not their messages. Write in the plain voice of
`../bro/SKILL.md`, and skip its restating step. Assume the user remembers their last message and nothing after it.
Call each PR, branch, child or job by a name they would know, not by a label you invented during the work.

## Gather what changed

Keep this turn read-only, except for visuals you make for the reply from what you already have. Do not change code,
start an app, a child or a job, or post anything to make them. Collect your own work since that message, results from
children, and messages from anyone else in the session. Then check the current state of each PR, CI run, child, task
or job this session touched, in one cell. Nothing here waits: never `rlm.collect`, which blocks on running children.

```python
import json
user = (await ctx.history(limit=200, kinds=["user"]))["items"]  # the last one is this /skill:ketchup message
since = user[-2] if len(user) > 1 else None
print("since:", f'{since["id"]} {since["preview"][:200]}' if since else "(no earlier user message in view)")

async def safe(name, call):
    try:
        return name, await call
    except Exception as error:  # an API this host does not offer, or a failed read: a gap to report
        return name, f"unavailable: {error}"

status = dict(await asyncio.gather(
    safe("children", rlm.results()),            # never waits: ended results plus ids still running (Rust host)
    safe("subagents", rlm.list_subagents()),    # every child with its state, on any host
    safe("tasks", agents.tasks()),              # typed agent tasks
    safe("background", background.list()),      # background jobs
    safe("shell jobs", rlm.jobs()),             # bash(..., yield_after=...) jobs
    safe("goal", goal.get()),                   # the /goal and where it stands
    safe("board", board.list()),                # the session's task board
    safe("inbox", swarm.inbox(mark=False)),     # unread agent messages, left unread
))
for name, value in status.items():
    print(name, json.dumps(value, default=str)[:600])
```

For each PR or branch the session touched, read its state through `bash` (`gh pr view <n> --json
state,mergeStateStatus,reviewDecision,statusCheckRollup`, `gh pr checks <n>`, `git log -1 <branch>`). A child that
succeeded counts as verified only when `result["check"]["outcome"] == "verified"`; say so for the rest. Say which
items are still running and which you could not check, and why.

An action item is a decision or step only the user can take, such as a product choice, an approval, an irreversible
action, or access you lack. Include any still waiting on the user, even one raised before their last message. Work you
can still do yourself is not an action item.

## Write the reply

Open with one sentence that says how many action items there are, or that there are none. If nothing happened and
nothing waits on the user, say so and stop.

Then say what happened, most important first. Give each result and what it means for the user. Skip steps that changed
nothing. Name in one line any work left that you can do yourself.

Then give each action item a short executive summary. Cover the decision to make, why it matters now, the options and
what each costs, your recommendation, and what happens if the user does not answer. If you do not know a part, say so
instead of guessing. Point to the detail when there is one (a PR URL, a file path), and write the summary so the user
can decide without opening it.

Show each action item as a before and after, side by side. For a choice, put the current state beside each option, or
the options alone when there is no current state. The reply renders in a terminal, so use a table, a diff, or a
plain-text diagram:

```
| Option           | Before          | After             | Cost           |
|------------------|-----------------|-------------------|----------------|
| A: retry 3 times | 4% of jobs fail | about 1% fail     | +2 s per retry |
| B: fail fast     | 4% of jobs fail | 4% fail, surfaced | none           |
```

When the change is visible, name the screenshot or recording file you captured by its path. If no visual exists yet,
make one from what you have. If you cannot, say plainly that none exists. Never invent a number, and never draw or
describe a screen you did not capture. Point only to files and PRs that exist.

Prefer a table, a diff or a diagram to dense prose.
