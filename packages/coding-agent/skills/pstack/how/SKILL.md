---
name: how
description: "Use for \"how does X work\", code walkthroughs before changing something, and placement / ownership / layering questions (\"where should this live\", \"which package owns this\", \"is this the right layer\"). Explains subsystem architecture, runtime flow, onboarding mental models. Use why for motivation."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/how
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# How

Explore the codebase to answer "how does X work?" questions. Produce an architectural explanation at the level of a senior engineer onboarding onto a subsystem: enough to build a working mental model, not so much that it reads like annotated source code.

You do the exploring and the explaining yourself, in your REPL. Search and read with code; use sub-model frames only for passages a line or two cannot settle. The explanation is your reply.

## Step 1. Scope

If the scope is ambiguous, state your interpretation and explore. The user can redirect.

- **Simple** (one module, a small utility, "how does function X work"): find it, read it, explain it.
- **Complex** (a subsystem spanning files or services, a cross-cutting feature, an architectural overview): decompose into 2 to 4 angles (entry points, the core flow, the data model, the boundaries) and work through each. Keep findings in `state["how"]` so they survive a kernel restart.

When in doubt, take the simple path.

## Step 2. Explore

Follow the code, don't guess from names:

1. **Find the entry point.** What triggers this behavior: a user action, an API call, a scheduled job?
2. **Trace the flow.** Follow the call chain from the entry point. Read each function. Note what data flows through and how it transforms.
3. **Map the key abstractions.** Read the definitions of the central types, services and classes.
4. **Find the boundaries.** What goes in, what comes out, which other subsystems it touches.
5. **Look for the non-obvious.** Historical artifacts, surprising behavior, what a newcomer would misread.

Narrow with code before reading. Search the concept and its synonyms, print one line per candidate, then read only the deciding regions:

```python
out = await bash('''rg -n --no-heading -e 'SessionManager' -e 'session_manager' src | head -40''')
print(out)
h = await rlm.load(path="src/core/session-manager.ts")
for m in h.search(r"export (class|function) \w+", limit=30):
    print(m["line"], m["text"])
print(h.lines(580, 610))
```

When many candidate files or long passages remain and a line or two cannot tell you which matter, judge them with frames, one passage per frame, under a contract:

```python
question = "How does a session resume after a crash?"
paths = ["src/core/session-manager.ts", "src/core/agent-session.ts"]
handles = [await rlm.load(path=p) for p in paths]
views = [v for h in handles for v in h.chunks(12000)]
notes = await rlm.map(
    f"For the question {question!r}: does this passage take part in the flow? If so, name the functions involved and what they do, citing file:line.",
    views,
    contract={"relevant": bool, "functions": list[str], "role": str},
)
state.setdefault("how", {})["notes"] = [n for n in notes if n and n["relevant"]]
```

Then read the relevant regions yourself before you describe them. A frame's summary is a pointer, not a citation.

Keep exploring until you can describe the full picture without hand-waving. If you cannot trace a part, say so: "I couldn't determine how X connects to Y" beats making something up.

### At most one subagent, only for subsystem scale

For a question that spans several packages and would need dozens of reads, you may hand the whole explanation to one read-only subagent and keep your own context clean. Never spawn explorers per angle or per file. The brief is self-contained: the question, the angles, the paths you already found, and `references/explainer-prompt.md` as the output format.

```python
skill_dir = "..."  # this skill's directory: "References are relative to <dir>" above
brief = str(await read(f"{skill_dir}/references/explainer-prompt.md")).replace("{QUESTION}", question)
brief += "\n\nRead-only: do not edit files. Paths found so far:\n" + "\n".join(paths)
h = await rlm.spawn(brief, name="how-explainer")
[res] = await rlm.collect([h])
```

Check the claims you will repeat against the code before presenting them.

## Step 3. Present

Write the explanation in the format of `references/explainer-prompt.md`, dropping sections that do not apply: Overview, Key Concepts, How It Works, Where Things Live, Gotchas. Use a mermaid diagram when components talk to each other or data moves through stages; skip it when prose covers the flow.
