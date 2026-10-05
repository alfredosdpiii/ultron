---
name: principle-guard-the-context-window
description: "Apply when context is filling up: large outputs, long files, repeated reads, fan-out planning. Keep bulk in REPL variables and handles; print summaries, not raw payloads."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/principle-guard-the-context-window
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Guard the Context Window

The context window is finite and non-renewable within a session. Every token should be worth its cost.

**Why:** Context overflow degrades reasoning quality, creates compression artifacts, and halts progress.

**Pattern:**
- **Isolate large payloads.** Keep verbose outputs and large documents in REPL variables or `rlm.load` handles, narrow them with code (`h.search`, counts, slices), and send what still needs reading to `rlm.map` frames. Print summaries, never raw data. Spawn a subagent only for independent multi-step work, never to read.
- **Keep frequently used content inline.** Templates and references used on every invocation belong in the skill file, not in separate files that cost a read each time.
- **Size phases and cap scope.** Limit files per phase, set turn budgets, account for mechanism costs.
