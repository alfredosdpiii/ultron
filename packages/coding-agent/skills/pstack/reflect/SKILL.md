---
name: reflect
description: Review the current session through three reviewer lenses (judgment, tooling, divergent) plus a synthesis frame, surface durable learnings, and route each to a concrete edit on an existing skill, AGENTS.md, or a tested code skill, applied only after the user approves. Use when the user says reflect.
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/reflect
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Reflect

Mine the current session for durable learnings, then route them into skill edits.

## When to invoke

Invoke when the user says "reflect" or "/reflect". Skip when the session is trivial, off-topic, or already covered by an existing skill you followed correctly. One-offs are not learnings.

## Process

### 1. Get the transcript

Read your own session file: it has every user message, cell and result in full. `scripts/sessions.py` from the recall skill (`../recall/scripts/sessions.py`) finds and parses it. The newest file for this working directory is usually this session; confirm that its first user message is this conversation's opening prompt. Never glob across other projects' sessions.

```python
import runpy
skill_dir = "..."  # this skill's directory: "References are relative to <dir>" above
s = runpy.run_path(f"{skill_dir}/../recall/scripts/sessions.py")
files = s["session_files"](days=2)
session = s["read_session"](files[0], tool_chars=1500)
print(files[0], session["messages"][0]["text"][:200])
transcript = "\n\n".join(f"[{i} {m['role']}] {m['text']}" for i, m in enumerate(session["messages"]))
h = await rlm.load(text=transcript, label="session")
print(h.size)
```

If no file matches, build it from your context instead: `items = (await ctx.history(limit=200))["items"]`, then `await ctx.get(item["id"])` for the items whose preview is not enough. As a last resort, write a tight digest of the session yourself and use that.

Turn numbers in findings refer to the `[i role]` markers.

### 2. Three reviewer frames

Run the three lenses as `rlm.infer` frames in one `asyncio.gather`, each with its template from `references/` as the task and the transcript as context. Use a different model per lens when more than one is configured, so the lenses do not share blind spots:

| Lens | Template | Model preference (a `find_models` query) |
|---|---|---|
| Judgment | `references/judgment-reviewer.md` | a strong reasoning model, such as "opus" |
| Tooling | `references/tooling-reviewer.md` | a different family, such as "gpt" |
| Divergent | `references/divergent-reviewer.md` | a third model if one exists, else the judgment model |

```python
async def pick(query):
    ms = await rlm.find_models(query)
    return f"{ms[0]['provider']}/{ms[0]['id']}" if ms else None

models = await asyncio.gather(pick("opus"), pick("gpt"), pick("gemini"))
models = [m or models[0] for m in models]
lenses = ["judgment", "tooling", "divergent"]
tasks = [str(await read(f"{skill_dir}/references/{lens}-reviewer.md")) for lens in lenses]
reviews = await asyncio.gather(*(
    rlm.infer(task, context=[h.lines(0, None)], contract=str, model=model)
    for task, model in zip(tasks, models)
))
for lens, review in zip(lenses, reviews):
    print(lens, "incomplete" if not review else f"{len(review)} chars")
```

A very long transcript may not fit one frame. Cut tool results harder (`tool_chars=400`) before you split it; split only by whole turns.

Frames have no tools. Each review lists references it wanted to check (tickets, threads, PRs the transcript cites). Look those up yourself (`gh` through `bash`, configured MCP servers through `mcp`) only where the answer would change a finding, and add what you found to the synthesis context.

### 3. Synthesize

One more frame with `references/synthesizer.md` as the task and the three reviews (and any lookups) as context. It returns the Accepted / Rejected / Backlog list. Before running it, read the skills the findings route to, and pass their text too: the synthesizer's already-covered check needs it.

```python
task = str(await read(f"{skill_dir}/references/synthesizer.md"))
labelled = [f"## {lens.title()} reviewer output\n\n{review}" for lens, review in zip(lenses, reviews) if review]
targets = []  # SKILL.md / AGENTS.md texts the findings route to, read with `await read(path)`
verdict = await rlm.infer(task, context=labelled + targets, contract=str, model=models[0])
state["reflect"] = {"session": session["id"], "reviews": list(zip(lenses, map(str, reviews))), "verdict": str(verdict)}
```

### 4. Structural enforcement check

Sanity-check the Accepted list. Any item a lint rule, test, script, metadata flag or runtime check would enforce more reliably moves to Backlog. See the encode-lessons-in-structure principle (`../principle-encode-lessons-in-structure/SKILL.md`).

### 5. Ask, then apply

Present the full Accepted / Rejected / Backlog output in your reply and end the turn. The user picks which rows to apply and may redirect routings. Skill and AGENTS.md changes affect every future session. Do not apply anything before the user answers.

For each approved Accepted row, follow its Routing exactly:

- **Trivial edit** to an existing SKILL.md or AGENTS.md (a bullet, a tightened sentence, a stale fact): `await edit(path=..., old_str=..., new_str=...)` after reading the file.
- **Substantive edit** (a new section, a pattern table, more than about 10 lines): draft it, show the draft, and apply it after the user approves it. Follow the skill format in Ultron's skills documentation (`docs/skills.md` in the coding-agent package).
- **`tune description: <skill path>`**: rewrite the frontmatter `description` so it front-loads the trigger words the session used, then `await skills.refresh()` and check `await skills.select("<the request that should have triggered it>")` lists it.
- **`new skill: <kebab-name>`**: write `<skills dir>/<kebab-name>/SKILL.md` (user skills live in `~/.ultron/agent/skills/`, project skills in `.pi/skills/` or `.agents/skills/`), then `await skills.refresh()`.
- **`code skill: <name>`** (a repeated procedure that is better as tested code than prose): `await skills.propose_code(name, source, test_source, evidence)`. It runs the test in a fresh kernel and activates the skill only on pass; check the result and `await skills.code_list()`.

Backlog items go to whatever tracker the user names; if none, list them in the summary.

### 6. Summarize for the user

Short list, no preamble:

- Edits applied: `<path>`. What changed, one line each.
- New skills or code skills: `<path or name>`. One line each (rare).
- Backlog: `<title>`. One line each.
- Dropped: one line per rejected finding with the synthesizer's reason.
