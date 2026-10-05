---
name: show-me-your-work
description: "Keep a reviewable decision trail for long-running or unattended work: a TSV log with one row per decision (what, why, evidence, result). Local by default; commit it when a reviewer needs the trail to trust the result. Use for /skill:show-me-your-work, /goal or multi-phase runs, or work a human reviews after stepping away."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/show-me-your-work
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Show me your work

Keep one canonical log.

## The format

A single TSV file, one row per decision. Cells stay single-line. Evidence is a pointer, not prose.

`references/decision-log-template.tsv` is the header row. Columns:

- **ts.** ISO8601 timestamp.
- **phase.** The phase or workstream.
- **decision.** What was chosen or done, one line.
- **why.** The reason in plain words. If a principle drove it, say it plainly, not as a jargon tag.
- **evidence.** A link or path that proves it: commit SHA, PR number, `file:line`, or an artifact, trace, or screenshot path. Never a paragraph.
- **result.** The outcome or predicate state: `tests green`, `reverted`, `pixel-diff 0`, `INCONCLUSIVE`, `open`.

An example, plain-spoken so a reviewer reads it at a glance.

```
ts	phase	decision	why	evidence	result
2026-05-24T09:02:00Z	frame	counted the work first, about 100 components and roughly 75 hours	wanted to know the size before starting a long run	commit 3a9f1c2	found 5 things to sort out before starting
2026-05-24T09:40:00Z	harness	took screenshots of the old version before changing anything	so we can compare old against new and catch any visual change	scripts/snapshot.sh, baseline/	saved 120 reference screenshots
2026-05-24T11:15:00Z	widget	moved the widget styles over without changing how it looks	keep the change small and the result identical	commit 7c21e0a, pixel-diff 0	looks identical, tests pass
2026-05-24T12:30:00Z	widget	threw out a child's work because its screenshots were blank	checked the real files instead of trusting its summary	worktree discarded	reverted, tightened the brief for next time
```

## Logging a row

Write each entry the way you'd tell a teammate what you did. Plain words, concrete actions, no AI speak or abstract jargon (`../unslop/SKILL.md` applies to log text too).

Define this helper in the REPL (again after a kernel restart; keep the path in `state["decision_log"]`). It stamps `ts`, writes the header on first use, flattens tabs and newlines, and prefixes any cell starting with `=`, `+`, `-`, or `@` with a single quote so a spreadsheet never runs it as a formula. Cells often come from generated or user-supplied text (PR titles, filenames), so don't hand-write rows around it.

```python
import datetime, os

def log_row(path, phase, decision, why, evidence, result):
    def clean(value):
        text = str(value).replace("\t", " ").replace("\r", " ").replace("\n", " ")
        return "'" + text if text[:1] in ("=", "+", "-", "@") else text
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    ts = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    with open(path, "a", encoding="utf-8") as f:
        if f.tell() == 0:
            f.write("ts\tphase\tdecision\twhy\tevidence\tresult\n")
        f.write("\t".join([ts, *map(clean, (phase, decision, why, evidence, result))]) + "\n")

state["decision_log"] = "decisions.tsv"
log_row(state["decision_log"], "frame", "counted the work first", "size it before a long run", "commit 3a9f1c2", "5 open questions")
```

From a shell script, `scripts/log.sh <logfile> <phase> <decision> <why> <evidence> <result>` (relative to this skill's directory) does the same; quote every argument with `shlex.quote` when calling it through `await bash(...)`.

Log decision points and checkpoints, not every action: a fork chosen, a unit completed with its verification result, a pivot or revert with its trigger, a blocker surfaced, a gate fixed. For a `/goal` run or any loop, one row per iteration. Skip the trivial and self-evident.

A run is one agent conversation, including its later turns and any compaction of it. A pickup, a subagent, a background job, or a new session starts a new run. When a run adds to a log that already has rows, its first row has phase `start`, and so does its first row after another run's `start` row. So a run that comes back to a log in a later turn first reads the log's last rows to see whether another run wrote since. A `start` row names the `ts` range of the rows before it that this run did not write, and its evidence names this run, such as its session file or subagent id. Use phase `start` for nothing else.

## Where it lives

By default the log is a working artifact, not committed. Keep it at `decisions.tsv` in the work dir, or `.audit/<task-slug>.tsv` when several efforts run at once, and leave it out of git.

Commit it only when the work is ambitious enough that a reviewer needs the trail to trust the result.

## Rules

- Append-only. A wrong call gets a new row that supersedes it. Never edit or delete history.
- Prefer evidence produced by committed scripts over hand-made one-offs (the encode-lessons-in-structure principle (`../principle-encode-lessons-in-structure/SKILL.md`)).

## Audit the log against the transcript

At the end of the run, before handing back, check the log told the truth against what this run actually did.

- **Recent turns:** `h = await ctx.history(limit=200)` lists what is still in your context (`h["items"]`, each with `id`, `kind`, `preview`); `await ctx.get(id)` gives one item in full.
- **The whole run, past compaction:** this session's file. `../recall/scripts/sessions.py` finds and parses Ultron's session files (native sessions under the agent dir's `experimental/sessions/`, Pi-format ones under `sessions/`, one folder per working directory). Several sessions can share a project, so pick the file that contains this run's own text (its first user message, a row you logged), not just the newest. Never read other projects' sessions: they are unrelated private chats.

```python
import runpy
s = runpy.run_path(f"{skill_dir}/../recall/scripts/sessions.py")  # skill_dir: the directory named in "References are relative to"
marker = "a phrase only this run wrote"
transcript = None
for path in s["session_files"](days=2):
    session = s["read_session"](path)
    if any(marker in m["text"] for m in session["messages"]):
        transcript = session
        break
```

Walk this run's rows against the transcript. Each stretch of them begins at one of this run's `start` rows, or at the first row if this run created the log, and ends at the next `start` row of another run:

- Check that every row maps to a real decision or action.
- Check that each row's evidence resolves and shows what the row claims (`git show`, `ls`, open the file).
- A fork, pivot, or abandoned approach that shaped the work but isn't logged is a gap. Add it.

Correct the log, not the story. The audit never edits or removes a row, even an invented one. When a row records neither a real decision nor a real action, or its claim or evidence is wrong, add a row that supersedes it with what actually happened and a pointer that resolves. This audit does not check rows outside this run's stretches. If this run's own work shows one of them is wrong, supersede it like any wrong call.

## Cross-model review of the trail

Before handing back, have a model from a different family than the one that did the work review the trail. Self-review is not a substitute. The reviewer is one `rlm.infer` frame: it reads the log and this run's transcript and flags what the user should pay attention to. Not a redo of the work, a scan for what's suboptimal or risky:

- Decisions logged with weak or absent evidence.
- Verification steps skipped or claimed without proof in the transcript.
- Choices that look risky in hindsight (premature, scope-creeping, papering over a symptom).
- Gaps the user would otherwise miss on a casual skim.

```python
ms = await rlm.find_models("gpt")  # query a family other than your own: "gpt", "gemini", "claude", ...
reviewer = f"{ms[0]['provider']}/{ms[0]['id']}" if ms else None
log = await rlm.load(path=state["decision_log"])
flags = await rlm.infer(
    "Review this decision log against the run's transcript. Flag weak or absent evidence, verification claimed "
    "without proof in the transcript, choices that look risky in hindsight, and gaps a casual skim would miss. "
    "Point each flag at a log row (its ts) or a transcript moment. Do not redo the work.",
    context=[log, transcript],
    contract={"flags": list[str]},
    model=reviewer,
)
```

If the transcript is large, pass only this run's stretch (`transcript.lines(a, b)` around the rows you audited) rather than the whole file. If `find_models` finds no other family, say so in the Attention section instead of reviewing your own work.

Every reply for a run that produced a trail ends with an "Attention" section. Lead with the reviewer's model on its own line (`reviewed by <provider/id>`), then list each flag pointing to specific rows or moments. "No flags" is a valid value. The model name is not optional.

## Reviewing the trail

Read top to bottom, follow the evidence pointers, spot-check. GitHub renders a committed TSV as a table. `column -s$'\t' -t decisions.tsv` renders it in a terminal.

## Composing this skill

Other skills route their audit trail here instead of inventing one. Reference it by name and let it own the format. Don't restate the columns.
