---
name: recall
description: "Reconstruct your recent working context from your own Ultron session history, long-term memory, live state, and the shared record (user reports, prior fixes, incidents), then hand back a tight current-state brief. Use for 'recall my work on X', 'catch me up', 'what have I been working on', 'where did I leave off', before starting or resuming work."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/recall
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Recall

**Before you start or resume work, you rebuild the user's recent working context and hand back a tight capsule of where things stand now and what to do next.**

Keep it tight and on-topic. Read only what the in-scope threads need, then stop.

Your context lives in two records. Your own session history holds what you did and decided. The shared record holds everything that happened around the same code under other names: the symptoms users keep reporting, the fixes that shipped and got reverted, the errors still firing in prod. That second record is what the **why** skill (`../why/SKILL.md`) searches, across source control, the issue tracker, team chat, long-form docs, and error tracking. A feature with a long bug tail keeps most of its story there, so don't reconstruct it from your sessions alone.

Sessions are JSONL files, one folder per working directory, under the agent dir (`ULTRON_CODING_AGENT_DIR`, default `~/.ultron/agent`): `experimental/sessions/` for native Ultron sessions and `sessions/` for Pi-format ones. `scripts/sessions.py` (in this skill's directory) finds and parses both; read its docstring once.

1. Classify, then route. Resuming one specific prior session is `/resume`, not this. Turning habits into a durable skill is `../automate-me/SKILL.md`. A human-readable summary of your work is a different task. Recall loads working context across recent sessions before you act. If the user already gave you a full state capsule (paths, branch, the change), use it and skip the mining.
2. Lock the scope before searching. Pin the window ("recent" is a real range, default the last 7 days), the topic if named, and the workspace (default the current working directory. Never read another project's sessions without being asked). State the scope back. Never quietly turn "all" into "recent N".
3. Mine your session history yourself, in the REPL. Order candidates by real modification time, grep the topic first, and read only the matching sessions and only their relevant regions. Skip the current session plus obvious noise: subagent sessions (`parent` set), sessions with no messages (inference frames only), and eval or test runs. Long matches go to `rlm.map` frames, one session region per frame, each returning the same schema. Raw transcripts stay in variables; only findings reach your context.

```python
import runpy, re
skill_dir = "..."  # this skill's directory: "References are relative to <dir>" above
s = runpy.run_path(f"{skill_dir}/scripts/sessions.py")
files = s["session_files"](days=7)  # current project; newest first by mtime
hits = s["grep_sessions"](files[1:], r"retry|backoff")  # files[0] is usually this session
print(len(files), "sessions;", hits[:10])
sessions = [s["read_session"](p) for p, _ in hits[:12]]
sessions = [x for x in sessions if x["messages"] and not x["parent"]]
regions = []
for x in sessions:
    text = "\n\n".join(f"[{m['role']}] {m['text']}" for m in x["messages"])
    h = await rlm.load(text=text, label=x["id"])
    spots = h.search(r"(?i)retry|backoff", limit=40)
    if spots:  # one region per session: from the first to the last match, with margin
        regions.append((x["id"], h.lines(max(spots[0]["line"] - 40, 0), spots[-1]["line"] + 40)))
THREAD = {"topic": str, "goal": str, "decisions": list[str], "open_threads": list[str],
          "struggles": list[str], "artifacts": list[str]}
briefs = await rlm.map(
    "From this session excerpt, extract the user's goal, decisions, open threads, struggles and corrections, "
    "and artifacts (PRs, tickets, branches, files) about: retry/backoff. Quote, don't guess.",
    [view for _, view in regions], contract=THREAD)
state["recall"] = {"scope": "last 7 days, this project, topic retry/backoff",
                   "threads": [dict(b, session=i) for (i, _), b in zip(regions, briefs) if b]}
```

For one or two sessions, skip the frames and read the matching regions directly (`print(view)`).
4. Check long-term memory: `await memory.prepare("<topic>", scope="project")` returns what earlier sessions in this project chose to keep (the default scope is this session only; if it raises, note memory as unavailable and move on). Treat it as a lead to verify, not as current state.
5. Sweep the shared record whenever the topic names a feature, file, subsystem, area, or bug. This is the default, not a judgment call, and "my work on X" does not exempt it. Use the **why** skill's discovery and per-source playbooks, but steer the question from "why was this built this way" to "what's the current state, what's been tried and didn't hold, and what are users still reporting". Inherit its posture: search every configured source, null results are findings, an unavailable or unauthenticated MCP server is skipped and named. Run the MCP searches together with `asyncio.gather` while you mine the sessions. Fold what comes back into the brief. Skip this step only for pure activity recall with no named target ("what did I do this week"), where your own history and live state are the entire answer.
6. Verify against live state. Take the PRs, branches, and tickets that the mining and the sweep surfaced and check them with `git` and `gh` through `bash` (`git branch --list`, `git log --oneline -5 <branch>`, `gh pr view <n> --json state,mergedAt,title`). When the answer hinges on what an agent actually did (the code it ran, files it read, errors it hit), read the full session region, not a frame's summary.
7. Write the brief to the contract below. Group by thread. Stay on the named topic.

## Output contract

Lead with the capsule, then the thread status, then the problems, then the next move. Deeper detail goes below or gets cut.

- **Capsule.** At most 5 bullets. What this work is and where it stands overall.
- **Threads.** One line each, prefixed with exactly one status tag: `[merged #N]`, `[open PR #N]`, `[in flight <branch>]`, `[verified, uncommitted]`, `[reverted #N]`, or `[planned, not started]`. A thread with no tag is not done yet, so tag it.
- **Problems.** At most 5, the recurring ones. Include the symptoms users keep reporting and any fix that shipped and was reverted, so the next attempt starts where the last one failed.
- **Next move.** The single most useful next action, concrete.

An adjacent feature or ticket stays out unless it blocks this one. When the capsule and thread lines outgrow a screen, cut detail before you cut threads. Write the brief through the **unslop** skill (`../unslop/SKILL.md`), cite session findings by session id and shared-record findings by their source (PR #, ticket ID, chat permalink, error-tracker issue), and sanitize private context before any public output.

**Reply:** the brief, to the contract above.
