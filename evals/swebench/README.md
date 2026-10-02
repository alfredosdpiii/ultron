# SWE-bench Verified harness

Runs three coding agents on the same SWE-bench Verified tasks, on the same model through the local CLIProxyAPI, and
scores their patches with the official SWE-bench evaluation.

| arm | what runs | API | reasoning effort |
| --- | --- | --- | --- |
| `ultron` | the installed `ultron` (`ultron --mode json -p --model cliproxyapi/gpt-6.1-sol`) | Chat Completions | medium (Ultron's default) |
| `codex` | the installed Codex CLI (`codex exec --json --sandbox danger-full-access`) | Responses | medium (set in its config) |
| `pi` | the installed stock Pi (`pi --mode json -p --model cliproxyapi/gpt-6.1-sol`) | Chat Completions | medium (Pi's default) |

Results are recorded under `acceptance/quality/` (`*-swebench-verified-*.json` and a `.md` rendering).

## Running it

Needs Docker, `uv`, and `ultron`, `codex`, `pi` and `node` on `PATH`. Everything the harness keeps lives outside the
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

`run` makes paid model calls (30 agent runs for the pilot). It is resumable: a (task, arm) pair with a `record.json`
is not run again, so rerunning the same command after an interruption continues where it stopped. To rerun a pair,
delete its directory under `runs/<run-id>/<arm>/`. Other flags: `--arms ultron,codex`, `--concurrency 2`,
`--limit-minutes 30`, `--only <id,id>`, `--memory 16g`, `--attempts 3`, `--skip-verify`, `--skip-eval`,
`--no-ledger`, `--out <file>`. Unknown flags exit 2 before anything starts.

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
still run on the testbed interpreter, through `bash()`.

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

## What a run leaves behind

```
~/.cache/ultron-swebench/
  venv/  dataset.jsonl
  runs/<run-id>/
    manifest.json  verify.json  pull-failures.json
    <arm>/<instance>/   record.json  patch.diff  stdout.jsonl  stderr.txt  ledger.jsonl  sessions/  (loki.jsonl)
    <arm>/predictions.jsonl
    eval/               the official evaluation's reports and logs
```

Containers are removed as each run ends (and by label when the harness exits). The pulled instance images stay;
they are reused by later runs and take a few GB each before layer sharing (`docker images 'swebench/*'`).
