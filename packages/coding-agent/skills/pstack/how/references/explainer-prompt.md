# Explanation format

The shape of a `how` answer. The root writes its reply to it; when the root hands a subsystem-scale question to one subagent, this file is that subagent's brief with `{QUESTION}` filled in.

---

You are writing an architectural explanation for a senior engineer who is new to this area. They should walk away with a mental model good enough to start working here confidently.

## Question

> {QUESTION}

## How to work

Read the code; don't guess from names. Narrow with search (`await bash('''rg -n ...''')`, `h = await rlm.load(path=...)`, `h.search`, `h.lines`) and read only the regions that decide the answer. Do not spawn subagents. You are read-only: do not edit files. If findings from earlier exploration are given below, reconcile them: merge overlaps and resolve contradictions by checking the code.

If you are a subagent, end with `await rlm.finish("done", summary, evidence=[...])` where `summary` is the explanation, and reply with it.

## Output format

Use this structure, adapted to the question. Drop sections that do not apply.

### Overview
One or two paragraphs. What this is, what it does, why it exists. A reader should be able to stop here and decide whether to keep going.

### Key Concepts
The types, services or abstractions needed to follow the rest. Brief definitions, not exhaustive.

### How It Works
The core and the longest section. What triggers it, what happens step by step, where data goes, where the decision points are.

Prose, not pseudocode. Name files and functions so the reader knows where to look; quote code only when a snippet is essential to the point.

When the flow involves components talking to each other or data transforming through stages, include a diagram: mermaid (```mermaid) for sequences, flowcharts and component graphs, ASCII for simple relationships. A diagram should clarify, not decorate.

### Where Things Live
A short file and directory map: only what someone needs to start working here.

### Gotchas
Non-obvious behavior, historical artifacts, pitfalls. Skip if there is nothing worth calling out.

## Style

- Concrete, not abstractions about abstractions: "`UserService` calls `AuthClient.refresh()`", not "the service delegates to the client".
- When something is complex, explain why it is complex. When it is simple, don't pad it.
- Use an analogy only if a good one exists.
- Name the gaps you could not trace instead of hiding them.
