---
name: create-verification-skill
description: "Generate a project-local verification skill (.pi/skills/verify-<app>/) that drives your app the way a user does, in any language, framework, or platform. Use for /skill:create-verification-skill, \"make a verify skill for this repo\", or when a project has no scripted way to prove UI/CLI/service behavior."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/create-verification-skill
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Create a verification skill

Every serious project needs a scripted way to drive the real app and prove behavior: launch it, exercise a feature the way a user would, and capture evidence. This skill generates that as a project-local skill, `.pi/skills/verify-<app>/`, tailored to the repo. Ultron discovers project skills in `.pi/skills/` and in `.agents/skills/` (from the working directory up to the repo root); use `.agents/skills/` instead if the repo already keeps its skills there. You write the generator's output for the next agent, not for a human: it will be read cold, mid-task, by an agent that has never seen the app and works through a Python REPL (`await bash(...)`, `read`, `edit`, `write`, `mcp`).

## 1. Interview the repo, not the user

Answer these from the codebase and only ask the user what you cannot observe:

- **Surface:** what does a user actually touch? A web UI, a CLI/TUI, a desktop app, an API, a mobile app, a library? A repo can have several; pick the primary one and note the rest.
- **Run:** how does the app start locally? Prefer the repo's own documented dev command (package scripts, Makefile, README quickstart). Note ports, env vars, seed data, auth.
- **Drive:** how can an agent interact with it programmatically? Existing harnesses first: Playwright/Cypress specs, expect scripts, PTY helpers, curl-able endpoints, a debug port. Only then pick a generic recipe: tmux through `bash` for a CLI or TUI, plain HTTP (`curl`) for services, and for web or Electron UIs a browser MCP server, only if one is configured (`await mcp.servers()`, `await mcp.search("browser")`). With no browser server and no repo harness, say so in the generated skill instead of inventing one.
- **Observe:** what evidence can be captured? Screenshots, terminal transcripts (`tmux capture-pane -p`), response bodies, logs, exit codes, DB state.
- **Isolate:** can two instances run side by side (ports, data dirs, profiles)? If not, say so in the generated skill: refusing to double-drive a shared instance beats corrupting the user's session.

If the checkout doesn't build or start as-is, fix that first (or report it precisely) before generating; a skill written against a broken base teaches wrong steps. When an irrelevant missing asset blocks startup (a static dir the API never serves, a sample config), the generated skill may create it, clearly marked as verification scaffolding, and remove it in cleanup.

## 2. Generate the skill

Write `.pi/skills/verify-<app>/SKILL.md` with YAML frontmatter (`name: verify-<app>` and a `description` that names the app, the surface, and when to reach for it; without frontmatter the skill never registers) and these sections, each grounded in what the interview actually found (no placeholders left). Write every recipe as REPL code the reader can paste: shell through `await bash('''...''')`, MCP through `await mcp.call(...)`.

- **Launch:** the exact command that starts the app for verification, and how to tell it's ready (a log line, a port answering, a prompt). Start a long-lived server so the cell returns: in a named tmux session, or as `await bash(cmd, yield_after=0)` with its PID recorded. Wait for readiness with one bounded command that blocks on the condition (`curl --retry 30 --retry-connrefused --retry-delay 1 -sf <url>`, `timeout 60 grep -m1 'ready' <(tail -f app.log)`), never fixed sleeps or repeated cells. Include teardown. For a short-lived CLI or TUI there is no server to keep alive: launch means build the binary (or install deps) once, then start each drive in its own isolated tmux session.
- **Doctor:** one read-only check that answers "is this instance worth driving?": process up, right version/build, port owned by us, auth valid. An agent runs this first whenever anything looks off.
- **Drive:** the harness recipe with real selectors/commands from this repo, not examples. Prefer stable handles (ARIA labels, data attributes, prompt strings, route paths) over coordinates and tab order. For a TUI, the shape is:

```python
import os
run = state.setdefault("verify_run", os.urandom(3).hex())
s = f"verify-{run}"
await bash(f'''tmux new-session -d -s {s} -x 120 -y 40 -e APP_DATA_DIR=/tmp/verify-{run} 'app --flag' ''')
await bash(f'''timeout 15 sh -c 'until tmux capture-pane -p -t {s} | grep -q "ready>"; do sleep 0.2; done' ''')
await bash(f'''tmux send-keys -t {s} 'search quarterly' Enter''')
screen = await bash(f'''tmux capture-pane -p -t {s}''')
```

- **Evidence:** what to capture for a proof and where it goes. State the proof standards: exercise the real user path, not internal setters or test-only endpoints; capture the action and the resulting state, not just the final screen; verify side effects (files written, rows inserted, messages sent) alongside what's visible; mocks only where a production boundary already isolates the external system. When the safe path is a dry-run or test mode, verify what it actually skips by observing (files, network, git refs) rather than trusting its name: some dry-runs still touch the network or open a browser.
- **Cleanup:** how to tear down instances the run created. Never kill by process name; kill what you started (`tmux kill-session -t <its name>`, the recorded PID). Cleanup removes instances and scratch state, never the evidence: proof artifacts survive the teardown, in a location the skill names.
- **Helpers:** any script the skill ships lives in the skill's `scripts/`, is executable, and its invocation is shown in the skill body. A helper the reader has to reverse-engineer is not a helper.

## 3. Seed the feature map

Create `.pi/skills/verify-<app>/features/README.md` plus one file per user-facing feature you can identify (aim for the top 3-5 to start, from routes, commands, menus, or docs). Follow the shape in `references/feature-map-example/`, with a README index and one file per feature. Each file answers, from the user's point of view: what the feature is, how to reach it, how to drive it with the harness, and what observable end state proves it works. The four H2s are `Sub-features`, `How to get to it (user POV)`, `Driving it with <harness>`, and `Gotchas`. The map is the repo's maintained verification source; a proof that drives one convenient entry point is incomplete when the map lists others.

## 4. Prove the generated skill before handing it over

Run its own instructions end to end once, from the REPL: launch, doctor, drive ONE mapped feature (one is enough; the map exists so later runs can cover the rest), capture evidence, clean up. After cleanup, confirm the evidence still exists at the named location (`ls` it); a cleanup that eats the proof fails this step. Fix what fails, and run the generated cleanup after every failed iteration too, so broken attempts don't strand processes, tmux sessions, and ports. A generated skill that was never executed is a draft, not a deliverable.

## 5. Offer the maintenance loop

Point the user at `/skill:maintain-verification-skill` (`../maintain-verification-skill/SKILL.md`) for keeping the map honest as the app changes. Suggest a cadence (`schedules`) only if they ask.
