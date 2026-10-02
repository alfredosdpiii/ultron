# SWE-bench Verified harness

Runs coding agents on the same SWE-bench Verified tasks, on the same model, and scores their patches with the
official SWE-bench evaluation. By default three agents on one model through the local CLIProxyAPI:

| arm | what runs | API | reasoning effort |
| --- | --- | --- | --- |
| `ultron` | the installed `ultron` (`ultron --mode json -p --model cliproxyapi/gpt-6.1-sol`) | Chat Completions | medium (Ultron's default) |
| `codex` | the installed Codex CLI (`codex exec --json --sandbox danger-full-access`) | Responses | medium (set in its config) |
| `pi` | the installed stock Pi (`pi --mode json -p --model cliproxyapi/gpt-6.1-sol`) | Chat Completions | medium (Pi's default) |

Two more arms compare `ultron --claude` with plain Claude Code on one model, Claude Opus 5.5, through the user's
Claude subscription. They run only when named (`--arms claude,ultron-claude`), in a run of their own:

| arm | what runs | login | effort |
| --- | --- | --- | --- |
| `claude` | the installed Claude Code, headless (`claude -p --output-format stream-json --verbose --model claude-opus-5-5 --strict-mcp-config --setting-sources "" --allowedTools Bash,Edit,Write,NotebookEdit`): its default system prompt and tools | the user's Claude Code login | Claude Code's default (medium for Opus 5.5 at the time of the pilot) |
| `ultron-claude` | the installed `ultron` on Claude Code (`ultron --claude --mode json -p --model claude-code/claude-opus-5-5`): Ultron's prompt, the REPL as the only tool | the same | Ultron's default thinking level, medium, passed as `--effort` |

Results are recorded under `acceptance/quality/` (`*-swebench-verified-*.json` and a `.md` rendering).

## Running it

Needs Docker, `uv`, and `ultron`, `codex`, `pi` and `node` on `PATH` (only the tools of the arms that run, and
Codex for its ripgrep). Everything the harness keeps lives outside the
repository in `~/.cache/ultron-swebench` (override with `ULTRON_SWEBENCH_HOME`).

```sh
node evals/swebench/run.mjs setup                       # virtualenv with swebench, dataset export (once)
node evals/swebench/run.mjs sample --n 10               # the task ids, without running anything
node evals/swebench/run.mjs verify                      # one tiny prompt per arm; proves model and effort
node evals/swebench/run.mjs run --run-id pilot10 --n 10 # the pilot: verify, 10 tasks x 3 arms, evaluate, report
node evals/swebench/run.mjs run --run-id pilot10 --n 50 --out acceptance/quality/<date>-swebench-verified-50-cliproxyapi_gpt-6.1-sol.json
                                                        # extend the pilot to 50 tasks: runs only the 40 new ones
```

The seeded order is prefix-stable, so the first 10 of the 50 are the pilot's tasks. Rerunning an existing run id
with a larger `--n` keeps the finished runs and adds the new tasks; a new `--run-id` with `--n 50` instead runs all
50 afresh (same tasks, new runs of the first 10).

```sh
node evals/swebench/run.mjs run --run-id pilot10-opus --n 10 --arms claude,ultron-claude \
  --out acceptance/quality/<date>-swebench-verified-pilot10-claude-opus-5-5.json
                                                        # the Claude arms: needs `claude` on PATH, logged in
```

`run` makes paid model calls (30 agent runs for the pilot). It is resumable: a (task, arm) pair with a `record.json`
is not run again, so rerunning the same command after an interruption continues where it stopped (a pair whose arm
was stopped, see the Claude arms below, is run again). To rerun a pair, delete its directory under
`runs/<run-id>/<arm>/`. Other flags: `--arms ultron,codex`, `--concurrency 2`, `--limit-minutes 30`,
`--only <id,id>`, `--memory 16g`, `--attempts 3`, `--skip-verify`, `--skip-eval`, `--no-ledger`, `--out <file>`,
`--ultron-package <dir>`, `--max-utilization 0.9`. Unknown flags exit 2 before anything starts.

`--ultron-package <dir>` runs the Ultron arm on another installed `ultron-agent` package than the one on `PATH`, for
a build under test. Install the build the way a user would, into a private prefix, and name the package directory:

```sh
npm run build:offline && node scripts/pack-release.mjs --out /tmp/ultron-build
npm install -g --ignore-scripts --prefix /tmp/ultron-build/prefix /tmp/ultron-build/ultron-*.tgz
node evals/swebench/run.mjs run --run-id pilot10-mybuild --n 10 --arms ultron \
  --ultron-package /tmp/ultron-build/prefix/lib/node_modules/ultron-agent
```

The run's manifest and result record that the version is a local build.

`eval --run-id <id>` reruns only the official evaluation and the report; `eval --run-id <id> --gold` evaluates the
gold patches of the run's tasks, which checks the images and the evaluation on this machine (expect all resolved);
`report --run-id <id>` only rewrites the report.

The unit tests of the pure parts (sampling, prompt, patch cleaning, usage parsing, aggregation, the recorder's
parsers) are `scripts/eval-swebench.test.mjs`, run by `npm run test:scripts`.

## Design

**Tooling and dataset.** `swebench` 5.0.2 from PyPI in a private virtualenv. swebench 5 reads the image name and the
eval script from the dataset row, so the dataset is `SWE-bench/SWE-bench_Verified`: the same 500 instances as
`princeton-nlp/SWE-bench_Verified` (ids, base commits, problem statements, gold and test patches and FAIL_TO_PASS
are identical; PASS_TO_PASS differs for two instances). Agents run in, and patches are evaluated in, the official
prebuilt instance images (`swebench/sweb.eval.x86_64.<instance>:latest`).

**Sample.** `sampleOrder` puts all 500 ids in one seeded order whose every prefix is stratified by repository
(systematic sampling: an instance's position is `(rank within its repository + a per-repository offset) / size of
the repository`). The pilot is the first 10 ids; a 50-task run takes the first 50, so it extends the pilot instead
of redrawing it. Seed `ultron-swebench-1`, fixed before the first draw. The run's `manifest.json` and the result
file record the seed and the ids.

**Where an agent runs.** One fresh container per (task, arm), started from the task's image with `sleep infinity`,
`--network host` (the proxy listens on 127.0.0.1) and a 16 GB memory limit, and removed afterwards. The repository
is `/testbed` at the task's base commit. The agent is started by a small script that activates the `testbed` conda
environment exactly as the official evaluation does, so `python` and the test runner in the agent's shell are the
project's own.

**Runtimes.** Nothing is installed into the images. The harness mounts read-only under `/opt/agent`, per arm: the
host's Node binary and the installed `ultron-agent` package; the Codex CLI's install directory; Pi's binary
directory; and, for every arm, a static ripgrep at the end of `PATH`. Ultron's REPL kernel and its Loki guardrails
need a modern Python while the task environments are Python 3.5 to 3.9, so the standalone CPython 3.12 the
virtualenv is built on is mounted too and named by `ULTRON_PYTHON` and `ULTRON_LOKI_PYTHON`; the repository's tests
still run on the testbed interpreter, through `bash()`, and Loki asks that interpreter (the active environment's
`python`, which Ultron passes to it) whether a newly imported module exists.

**Prompt.** One prompt for every arm (`buildPrompt`): the issue text (`problem_statement`), where the repository
is, that the environment is active, and that hidden tests score the change. No hints, no gold or test patch, no
names of failing tests. It asks the agent not to look up the upstream fix; commands that touch the network are
listed per run (`networkCommands`) for a later look, since `--network host` cannot block them.

**Isolation.**

- Agent state lives in `/agent` inside the container and dies with it: `ULTRON_CODING_AGENT_DIR`,
  `ULTRON_SERVER_DIR`, `PI_CODING_AGENT_DIR` and `CODEX_HOME` all point there. Nothing of `~/.ultron`, `~/.pi` or
  `~/.codex` is mounted or copied, except the `cliproxyapi` provider entry of `~/.ultron/agent/models.json` cut to
  the one model, with `"apiKey": "$CLIPROXY_API_KEY"` in place of the key.
- The key is read from `models.json` at run time and passed as `CLIPROXY_API_KEY` in the environment of
  `docker exec`. It is never written to a file; the evidence copied out of a container is scrubbed of it.
- Ultron: a new user's configuration (no settings.json, skills, AGENTS.md or extensions), Hindsight off, Loki guard
  on with the bundled engine and `ULTRON_LOKI_AUTOINIT=off` so no `.loki/` is created or committed; `.loki/` is
  excluded from predictions regardless.
- Codex: a fresh `CODEX_HOME` with only `config.toml` (a `model_providers` entry on the proxy, `wire_api =
  "responses"`, `env_key`). Its own sandbox is off (`danger-full-access`): bubblewrap and the legacy Landlock
  sandbox both fail in an unprivileged container, and `workspace-write` then fails every command. The container is
  the sandbox; it gets no extra privileges. Codex 0.152.1 has no catalog entry for `gpt-6.1-sol` and runs it on its
  fallback model metadata (it warns about this at the start of every run).
- Pi: a fresh agent dir with only `models.json`.

**Limits.** A wall-clock limit per run (default 30 minutes, `timeout` inside the container plus a backstop in the
harness) and otherwise each tool's defaults. Two runs at a time by default. A run that ends in a provider error
(429, proxy or upstream failure) is retried after a backoff, up to three attempts, and a rate-limited run delays
the next start by a minute; a crash before the first model turn is retried the same way.

**Prediction.** After the agent ends (or is stopped at the limit), everything left running in the container is
killed and the prediction is `git add -A && git diff --cached <start commit>` in `/testbed`, with `.loki/` and
binary files dropped (`cleanPatch`). A run stopped at the limit still yields its working tree as the prediction.

**Usage.** Turns, tool calls and tokens come from each tool's own output: Ultron's session journal (`usage` rows,
sub-agent sessions included), Pi's `--mode json` events, Codex's `turn.completed` usage and rollout file. Tokens
are normalised to uncached input, cached input and output. In addition every request passes through a pass-through
recorder (`recorder.mjs`) that keeps a per-run ledger of the model asked for, the model that answered, the
reasoning effort, the status and the upstream usage block: the proof that an arm ran on `gpt-6.1-sol`, and a
second token count taken identically for all arms. Cost is notional: tokens at the list prices in `models.json`;
the proxy bills a subscription.

**Scoring.** `python -m swebench.harness.run_evaluation` on each arm's `predictions.jsonl`. A task is resolved only
on that verdict. The report separates harness failures (image pull, container start, provider errors, agent
crashes, evaluation errors) and timeouts from genuinely unresolved tasks.

## The Claude arms

**What is compared.** The same model, the same Claude Code binary (mounted read-only like the other CLIs), the same
login, the same prompt. The `claude` arm is a fresh install's Claude Code: default system prompt, default tools,
default permission mode and default effort. The `ultron-claude` arm is a new user's Ultron (as the `ultron` arm:
fresh agent dir, Hindsight off, Loki on without `.loki/`) with Claude Code as the model of its root lane: Ultron
starts `claude -p` with no built-in tools, its own system prompt and its REPL as the one MCP tool.

**No permission bypass.** The containers run as root, and Claude Code asks before it runs a command or edits a
file. The `claude` arm allows those tools by name (`--allowedTools Bash,Edit,Write,NotebookEdit`) and nothing
bypasses permissions; what would still ask (WebFetch, WebSearch) is denied, since print mode has nobody to ask, and
shows in the run's `permissionDenials`. Ultron allows only its own MCP tool.

**Vanilla.** `CLAUDE_CONFIG_DIR` is a fresh `/agent/claude-config` in the container, so none of the user's
CLAUDE.md, settings, hooks, skills, plugins, MCP servers or history exists there, and the sessions Claude Code
writes land there, not in the user's config dir. `--strict-mcp-config` and `--setting-sources ""` (Ultron passes
both too) keep project settings and MCP servers out as well.

**The login.** Two ways, in this order, and never a third:

1. `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`), when the harness's environment has it: named in the
   environment of `docker exec`, never written anywhere.
2. Otherwise the one file `<the user's Claude config dir>/.credentials.json`, bind-mounted **read-only** into the
   fresh config dir. Nothing else of that directory is mounted, and the harness only reads the file.

Read-only is the mode that cannot damage the login: in a container a single-file mount cannot be replaced (a rename
over it fails with EBUSY) and cannot be written (EROFS), while a read-write mount would let Claude Code's fallback
truncate and rewrite the host's file in place, racing the user's own sessions. What no mount mode makes safe is a
token refresh inside a container: it rotates the refresh token on the server, and the copy the host keeps goes
stale. So the harness never lets it happen: before every run it reads the access token's expiry and starts the run
only if the token outlives the run's limit plus 15 minutes; otherwise the run is a harness failure
(`auth_unavailable`) and the Claude arms stop. The token is refreshed by any Claude Code session on the host.
The evidence copied out of a container (transcripts, sessions) is scrubbed of both tokens; the config dir itself is
never copied.

**The subscription.** Claude Code reports how full the account's usage windows are (`rate_limit_event`). A run that
ends on the usage limit, is served from paid overage, or sees a window at 100% is a harness failure (`usage_limit`,
never "unresolved") and stops both Claude arms; the tasks they did not start are recorded as `not_run`. A run does
not start either once a window is 90% used (`--max-utilization`): the rest is the user's, and overage is real
money. Rerunning the same command after the window resets runs what is missing.

**Verification.** Without the recorder (nothing sits between Claude Code and its API), the proof of model and
effort is Claude Code's own: the stream's `init` and message models and the result's `modelUsage`, and the session
transcripts it writes (`projects/**.jsonl`), where every response carries its model and the effort it ran at.
Verification fails unless every response is `claude-opus-5-5`, on the subscription login (`apiKeySource: none`),
and both arms ran at the same effort.

**Usage.** `claude`: turns are the model's responses, tool calls the `tool_use` blocks, tokens and cost the CLI's
totals in the final `result` event (cache writes counted apart from uncached input). `ultron-claude`: Ultron's
session journal as for the `ultron` arm (cells, helpers, frames, sub-agent sessions, tokens, the cost the CLI
reported), Loki's log, and `ultron usage --json` of the root session (`usage.json`). For both, the transcripts
give a second token count taken the same way. Cost is what Claude Code reports, notional on a subscription.

## What a run leaves behind

```
~/.cache/ultron-swebench/
  venv/  dataset.jsonl
  runs/<run-id>/
    manifest.json  verify.json  pull-failures.json
    <arm>/<instance>/   record.json  patch.diff  stdout.jsonl  stderr.txt  ledger.jsonl  sessions/  (loki.jsonl)
                        (Claude arms: claude-projects/ for Claude Code's transcripts, usage.json; no ledger)
    <arm>/predictions.jsonl
    eval/               the official evaluation's reports and logs
```

Containers are removed as each run ends (and by label when the harness exits). The pulled instance images stay;
they are reused by later runs and take a few GB each before layer sharing (`docker images 'swebench/*'`).
