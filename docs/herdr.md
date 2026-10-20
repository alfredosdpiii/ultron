# Ultron in herdr

When Ultron runs inside a herdr pane, it shows up in herdr's agents panel.
herdr-gpui is a client of the same herdr daemon, so the same entry appears there too. There is nothing to install:
Ultron sees the `HERDR_ENV=1`, `HERDR_SOCKET_PATH` and `HERDR_PANE_ID` variables herdr sets in the pane and reports
to herdr's socket.

## What shows

| Mode | Agent shown | Session | Metadata |
| --- | --- | --- | --- |
| `ultron`, `ultron --claude` (TUI) | Ultron | the session id and file; resume command `ultron --session <id>` | title (the session name, or the first prompt), `model`, `ctx` (context used), `cost` (session $) |
| `ultron-s` (TUI) | Ultron | the durable session; resume command `ultron-s` (the home session) or `ultron-s --session <id>` | as the TUI |
| `ultron claude` | Ultron (Claude) | the Claude Code session; resume command `ultron claude --resume <id>` | none |
| `ultron -p`, `--mode json`, `--mode rpc` | Ultron | none | none |

The resume command is what herdr runs to bring the session back after a herdr restart. When `ULTRON_SERVER_DIR` or
`ULTRON_CODING_AGENT_DIR` is set, the command starts with `env VAR=...` so it opens the same instance.

## States

- **working**: a run is in progress, including the persona's auto-nudge and keep-going continuations that start
  right after a run ends.
- **blocked**: Ultron is waiting for you. Either the run's final paragraph ends with a question, or the run failed
  on an auth, model or usage-limit error (the message says which). In `ultron claude`, a permission prompt or Claude
  Code's idle prompt also counts.
- **idle**: the run is finished and nothing is asked. After an abort, or after a failure that is not one of the
  errors above, the message says what happened.

The agent is removed from the panel when Ultron exits, whether you quit normally, press Ctrl-C or the process gets
a signal. In `ultron claude` it is removed when the Claude Code session ends.

## Turning it off

Either of these turns the integration off:

- `"herdr": false` in the global `settings.json` (the TUI and the prompted modes also read it from a trusted
  project's `.pi/settings.json`);
- `ULTRON_HERDR=0` in the environment.

Reporting never slows the TUI down. A background thread sends every report with 0.5 s timeouts and ignores
errors, so a herdr that is gone or stuck has no visible effect. Set `ULTRON_DEBUG_INTERNAL=1` to print the first
error to stderr.

## Limits

- `herdr agent start --kind` and herdr-gpui's agent fan-out only know herdr's built-in agent kinds. They cannot
  start Ultron. Open a pane and run `ultron` (or `ultron claude`) yourself; once it runs, it shows in the panel like
  any other agent.
- `ultron claude --no-hooks` registers no hooks, so `ultron claude` then reports nothing.
- A `ultron -p` started from inside an Ultron session's REPL does not report, so it cannot take over the pane of
  the session that runs it.
