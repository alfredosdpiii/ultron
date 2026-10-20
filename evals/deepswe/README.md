# DeepSWE swarm A/B harness

Runs ultron-rs on long-horizon tasks from DeepSWE (`datacurve/deep-swe`: 113 tasks in Harbor format, prebuilt
offline images, program verifiers) and compares two arms on the same model:

| arm | what runs |
| --- | --- |
| `ultron` | the portable ultron-rs binary, default prompt: `ultron --mode json -p --model cliproxyapi/gpt-6.1-sol "<problem statement>"` |
| `ultron-swarm` | the same, plus `swarm-directive.md` written as `APPEND_SYSTEM.md` into the run's agent dir |

The swarm arm changes only the instructions. It does not use `--append-system-prompt`: the frozen binary
(cf87c0643) drops that flag; `<agent dir>/APPEND_SYSTEM.md` is read (checked with a canary on 2026-10-20).

Results go to `acceptance/quality/<date>-deepswe-swarm-<n>-cliproxyapi_gpt-6.1-sol.{json,md}`. They hold task ids,
pass/fail, times, tokens and swarm-call counts only. **Never commit dataset content** (problem statements,
patches, verifier scripts, task text): DeepSWE is gated, held-out evaluation data.

## Running it

Needs Docker, `uv`, `node`, the gated parquet (default `../deepswe/ds/data/test-00000-of-00001.parquet` next to the
checkout, or `ULTRON_DEEPSWE_PARQUET`), the proxy entry `cliproxyapi` in `~/.ultron/agent/models.json`, and
`uv python install 3.12`. Everything the harness keeps lives in `~/.cache/ultron-deepswe`
(`ULTRON_DEEPSWE_HOME`), outside the repository.

```sh
node evals/deepswe/run.mjs export                 # the parquet rows into ~/.cache/ultron-deepswe/tasks.jsonl
node evals/deepswe/run.mjs gold                   # grade every reference patch; all must pass
node evals/deepswe/run.mjs smoke                  # one tiny prompt per arm in one image (paid, two calls)
node evals/deepswe/run.mjs run --run-id ab4       # 4 tasks x 2 arms, concurrency 2, grade, report (paid)
node evals/deepswe/run.mjs grade --run-id ab4     # regrade the run's patches
node evals/deepswe/run.mjs report --run-id ab4 [--date 2026-10-20] [--out <file>]
```

Flags: `--arms ultron,ultron-swarm`, `--only <id,id>`, `--concurrency 2`, `--limit-minutes <m>` (default the task's
`agent_timeout_sec`, 90 min), `--attempts 2`, `--skip-gold`, `--binary <path>`. Unknown flags and commands exit 2.
`run` is resumable per (task, arm): a pair with a `record.json` (other than a harness error) is not run again, and
an ungraded one is graded. To rerun a pair, delete `runs/<run-id>/<arm>/<task>/`.

The default tasks: `sqlfmt-create-table-ddl-formatting` (Python), `arcane-drift-detection-baselines` (Go),
`kysely-window-grouping-helpers` (TypeScript), `wasmi-trap-coredumps` (Rust). The Python task was meant to be
`narwhals-rolling-window-suite`; its gold check fails in its image (`test.sh base` has 6 failures unrelated to the
task: polars DeprecationWarnings in the interchange tests and a get_column assertion), so it was swapped for another
multi-file Python feature request whose gold passes.

## The portable binary

The frozen release build (`eval-frozen/ultron-cf87c0643`) links glibc 2.43, newer than the task images'. The harness
runs `eval-frozen/ultron-cf87c0643-portable`: the same commit built as a static musl binary on the host, with zig as
the C compiler for `ring` and `aws-lc-sys`:

```sh
rustup target add x86_64-unknown-linux-musl
# cc-musl: a wrapper that drops cc-rs's --target=x86_64-unknown-linux-musl (zig cannot parse it) and runs
#   exec /usr/bin/zig cc -target x86_64-linux-musl -fno-sanitize=undefined "$@"
# (zig turns UBSan on by default; without the flag aws-lc's objects need the UBSan runtime and the link fails).
# The link itself uses the host's cc with Rust's self-contained musl CRT.
CC_x86_64_unknown_linux_musl=cc-musl AR_x86_64_unknown_linux_musl="zig ar wrapper" \
  ../ultron-rust-wt/tools/cargo.sh build --release --locked -p cli --bin ultron --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/ultron ../eval-frozen/ultron-cf87c0643-portable
```

The result is a static-pie binary (`ultron --version`: 0.87.39) that runs in any x86-64 image and on the host.

It is mounted read-only at `/opt/ultron/ultron`. Nothing is installed into the images.

## Design

**Where an agent runs.** One fresh container per (task, arm) from the task's `docker_image`, `sleep infinity`,
`--memory`/`--cpus` from the task row, named and labelled `ultron-deepswe-<run>-...` and removed afterwards. The
agent runs in `/app` (Harbor's repository dir) at the task's base commit. The agent container uses
`--network host`, because the proxy listens on 127.0.0.1:8317: **this departs from the tasks'
`allow_internet = false`**. The grading containers run with `--network none`.

**Kernel Python.** The REPL kernel needs a modern Python; the harness mounts uv's python-build-standalone CPython
3.12 read-only at `/opt/ultron/python` and names it by `ULTRON_PYTHON`. The task's own toolchain (its `python3`,
`go`, `node`, `cargo`) is what the agent's `bash()` finds on `PATH`.

**Isolation.** `ULTRON_CODING_AGENT_DIR`, `ULTRON_SERVER_DIR` and `HOME` are fresh dirs under `/agent` in the
container and die with it. The agent dir holds only `models.json`, the `cliproxyapi` provider cut to
`gpt-6.1-sol` with `"apiKey": "$CLIPROXY_API_KEY"`, and for the swarm arm `APPEND_SYSTEM.md`. No `auth.json` is
copied: the proxy model needs none. The key is passed as `CLIPROXY_API_KEY` in the environment of `docker exec`,
never written to a file, and the evidence copied out is scrubbed of it. Hindsight is off
(`ULTRON_HINDSIGHT_URL=off`) and Loki does not create `.loki/` (`ULTRON_LOKI_AUTOINIT=off`).

**Prompt.** The row's `problem_statement` verbatim (Harbor's `instruction.md`), nothing added.

**Limits.** `agent_timeout_sec` (5400 s) through `timeout` in the container, plus a backstop in the harness. A run
that crashes before its first turn or ends on a provider error is retried once after a minute.

**Prediction.** After the agent ends, the container's leftover processes are killed by pid (never by group), and
the prediction is `git add -A && git diff --cached <base_commit>` in `/app`, without `.loki/`, `.ultron/`, `.pi/`
and binary sections.

**Grading, as Harbor does it.** A second fresh container from the same image, no network: the task's
`tests/test.sh` (the row's `verifier_script`) and `tests/test.patch` are written to `/tests`, the prediction is
`git apply`'d in `/app` (a patch that does not apply is `patch_failed`), `bash /tests/test.sh` runs under the
row's `verifier_timeout_sec`, and the reward is `/logs/verifier/reward.txt` (1 = pass). The verifier resets the
files the test patch touches, applies it, and requires both `test.sh base` and `test.sh new` to pass. `gold` grades
each `reference_patch` the same way; `run` refuses to start until gold passed for its tasks.

**Usage.** Sub-agents run as lanes of the root session's worker, and in 0.87.39 their responses are not in the
session file's `usage` rows: `ultron usage <session> --json` (schema `ultron.session-report/1`) reports the root
lane only (its sub-agent and frame lanes read zero even when children ran). The whole tree's usage is in the host's
budget ledger, the last `ultron.usage` / `root` value of the journal: per budget root, responses, tokens and cost.
A turn's root (`turn:<id>`) carries the root lane plus children spawned inside the turn; kept children
(`keep=True`) outlive the turn and are charged to the session root (`ultron-root`). So:

- tree responses, tokens and cost: the ledger, all roots summed (cost at the `models.json` list prices: notional, the
  proxy bills a subscription; no cached/uncached split is available for children);
- root lane: the session report's root lane (run on the host against the copied agent dir);
- children: tree minus root lane; children spawned: the host's `rlm.spawn` count;
- swarm calls: the host's own counters (`hostCalls` of the session's `ultron.module` stats, children's calls
  included), kept for `swarm.*`, `board.*`, `notes.*`, `rlm.spawn`, `rlm.ask`, `rlm.partial` and `rlm.collect`;
  beside them, the `countSwarmCalls` regex count (scripts/eval-quality.mjs) of the root's rlm code.

## What a run leaves behind

```
~/.cache/ultron-deepswe/
  tasks.jsonl  gold/<task>/{grade.json,grade.log}  gold/gold.json  smoke/
  runs/<run-id>/manifest.json
  runs/<run-id>/<arm>/<task>/  record.json  patch.diff  stdout.jsonl  stderr.txt  usage.json  agent/ (sessions)  grade/
```

The pulled images stay (`docker images public.ecr.aws/d3j8x8q7/swe-bench-202605`).

The unit tests of the pure parts are `scripts/eval-deepswe.test.mjs` (`node --test scripts/eval-deepswe.test.mjs`).
