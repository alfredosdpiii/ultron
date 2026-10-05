---
name: automate-me
description: "Use for \"automate me\", \"create/update/refresh my -mode skill\", \"turn/capture my preferences or working style into a skill\", or wanting agents to follow how the user works. Drafts or revises a personal -mode skill from the user's own Ultron session history and their answers, written through unslop."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/automate-me
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Automate me

Turn the user's working conventions into one `-mode` skill agents will follow (e.g. `jay-mode`). Mine their sessions, ask them, cluster, draft, iterate. Prose discipline comes from `../unslop/SKILL.md`; skill format from Ultron's skills documentation (`docs/skills.md` in the coding-agent package).

## Flow

### 0. Check for an existing skill

Look for `*-mode/SKILL.md` matching the user's handle under `~/.ultron/agent/skills/`, `~/.agents/skills/`, and the project's `.pi/skills/` and `.agents/skills/` (recursively: a personal category directory counts). If one exists and the user didn't already say "update", ask in your reply whether to update it (the default) or start fresh (ask why), and end the turn.

Update mode: mine only sessions since the file last changed (its mtime, or `git log -1 --format=%cI <path>` in a repo); ask what changed or is missing; edit in place, keeping sections the user hasn't contradicted.

### 1. Mine their history

Only the current project's sessions unless the user names others: reading other projects' sessions crosses into unrelated private work. Use `../recall/scripts/sessions.py`, take the last 2 to 4 weeks, and split them into 3 time slices so a signal can be checked for recurrence. Read only the user's messages and the turns right after a correction; frames summarize each slice.

```python
import runpy
skill_dir = "..."  # this skill's directory: "References are relative to <dir>" above
s = runpy.run_path(f"{skill_dir}/../recall/scripts/sessions.py")
files = s["session_files"](days=28)[1:]  # skip this session
slices = [files[i::3] for i in range(3)] if len(files) >= 6 else [files]
def user_turns(paths):
    out = []
    for p in paths:
        x = s["read_session"](p, tool_chars=300)
        if x["messages"] and not x["parent"]:
            out += [f"[{x['id']}] {m['text'][:1500]}" for m in x["messages"] if m["role"] == "user"]
    return "\n\n".join(out)
SIGNALS = {"patterns": list[str]}  # each: "<area>: <pattern> (evidence: <session id>, <short quote>)"
found = await rlm.map(
    "List the user's recurring working conventions in these messages: response preferences (length, tone, "
    "format, 'dumb it down' corrections), delegation habits (subagents, models, parallelism), verification "
    "posture (what 'done' means), code and prose discipline, process conventions (worktrees, commits, PRs), "
    "meta preferences (fixing skills mid-task). Only patterns with evidence; quote it.",
    [await rlm.load(text=user_turns(sl)) for sl in slices], contract=SIGNALS)
```

Cross-check across slices. A pattern in 2+ slices is high-confidence; a lone signal is weak and usually dropped. Also check `await memory.prepare("user preferences", scope="global")` for kept facts; if it raises, skip it.

### 2. Ask the user directly

Mining misses intent that hasn't come up yet. In one reply, ask one or two short questions with 4 to 6 numbered options each ("Which areas matter most? Pick any: 1. response style ..."), plus one free-form question for anything the options miss. End the turn and wait. Don't dump 20 questions.

### 3. Cluster

Group the signals into sections, only those that apply: response style; autonomy; understand first (which skills to reach for when scoping); subagents (when to spawn, models, parallelism); prose and code discipline; review and verify; process (worktrees, commits, PRs); skills (fix-the-skill-first, proposing new ones). `../rigor/SKILL.md` shows the granularity. Don't copy its content: the user's rules are their own.

### 4. Draft

- Path: keep an existing mode skill's location. For a new one, `.pi/skills/<handle>-mode/SKILL.md` in the project, or `~/.ultron/agent/skills/<handle>-mode/SKILL.md` if the user wants it everywhere.
- `description`: triggers on their name, `/skill:<handle>-mode` and "work in their style", not generic keywords like "write code". One YAML scalar; quote it when punctuation needs it.
- `disable-model-invocation: true` unless the user wants it on every turn.

### 5. Iterate

Write every line through unslop. Show the draft in your reply, take feedback, cut. A mode skill is not a manual. After writing it, `await skills.refresh()`.

### 6. Land it

In a repo: work on a branch or worktree off main and open a PR with `gh`; don't push to main. Commit only when the user asks.

## Guardrails

- **Don't overfit to one conversation.** A preference stated once and contradicted another time is noise.
- **Keep it operational.** No restating other skills, no metaphors, no poetic prose for an agent reader.
- **Reference, don't inline.** Other skills and principle docs appear as paths.
- **Minimal sections.** Only where the user has a specific, non-default rule. "Communicate clearly" is not a section; "Short paragraphs. Tables when comparing options." is.
- **Generic imperatives.** "The user", not the person's first name.
- **No forced symmetry.** No process rules worth writing down means no Process section.

Evaluate by asking the user: does it read like them, did it miss anything. Tune the description only if triggering turns out to be a problem.

## When not to use

A task-specific skill or one narrow workflow ("how I write commit messages") is a regular skill: write it directly, no mining.
