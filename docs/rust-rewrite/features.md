# Rust-only features

Features of ultron-rs that the TypeScript product does not have (wave 38, ideas taken from reflection-oss/mirror).
Each keeps the RLM-first rule: the model has one tool, rlm; new abilities are kernel functions, host guards or CLI
commands, and anything added to the model's context is a short pointer.

## Typed memory records, file-linked pointers, memory import (wave 38, w14-memory)

Ideas from reflection-oss/mirror (`memory_record.rs`, `memory/import/*`), fitted to Ultron's RLM-first memory: the
model still reaches memory only through the `rlm` REPL (`memory.*`), and the host adds at most short pointer lines.

**Typed records.** `memory.propose(text, evidence, *, scope, type=, key=, situation=, path_globs=)`. `type` is
`preference | fact | procedure | pitfall | reference`; `situation` (required with a type) is one line of at most
200 bytes saying when the record applies; `key` is a kebab-case handle of at most 64 bytes; `path_globs` is at most 16
repository-relative patterns (`*`, `?`, `**` as a whole segment, `[set]`, `[!set]`, `{a,b}`; no absolute paths,
`..` or `//`). Content caps by type: pitfall 1 KiB, preference 2 KiB, fact and reference 4 KiB, procedure 8 KiB.
Malformed fields fail as `Memory operation did not complete (INVALID_INPUT)`; a field of the wrong JSON type or an
unknown type name fails the worker schema (`Invalid memory request: memory.propose`). Situation, key and globs go
through the same secret check as the text (a hit stores nothing: state `sensitive`). Untyped records are unchanged.

The text stays in Hindsight and the session journal (format unchanged, hashes only). Typed metadata (id, type, key,
situation, globs, scope, identity tags, text hash, source) is kept in `<agent dir>/memory/records.json`, shared by
every session and locked (`records.json.lock`) on each change. Proposing a key that is already indexed in the same
scope replaces that record: a `correct` when this session wrote it, otherwise a new claim on the same Hindsight
document. `memory.get(id)` reads an indexed record of another session (same scope tags) from Hindsight directly; that
read is not journaled because the session journal only holds its own documents. `forget` removes the index entry.
Results of a typed write or read carry `record` (`type`, `key`, `situation`, `pathGlobs`, `source`, and `indexed`).

**File-linked pointers.** The session's file hooks observe every cell (`host::file_hooks::CellObserver`). When the
files a cell wrote (checked `write()`/`edit()` writes and any change the after-cell snapshot finds) match a visible
record's `path_globs`, the lane's next cell result ends with at most 3 lines `[memory] <id> <type>: <situation>`. Each
record is pointed at once per session, and the content is never included: the model calls `memory.get(id)`. Reads
are not observed (the kernel reads files without a host request). Turn it off with the setting
`"memory": {"filePointers": false}`; it is also off when memory is off.

**Import.** `ultron memory import --from claude-code|codex [--dry-run]` reads, as Mirror does, Claude Code's
`~/.claude/projects/<project>/memory/*.md` notes (`CLAUDE_CONFIG_DIR` moves `~/.claude`; `MEMORY.md`, dotfiles,
symlinks and `old/` skipped; frontmatter `name`, `description`, `metadata.type`) and Codex's
`~/.codex/memories/memory_summary.md` and `MEMORY.md` (`CODEX_HOME` moves `~/.codex`; bullets per section). Each
note becomes a typed record with `source` naming the harness and home-relative file, evidence that file's sha256 and
evidence class `user_statement`, through `memory.propose` (same secret check). A Claude Code project directory whose
encoded name resolves to exactly one existing directory gives `project` records of that directory; everything else is
`global`. Keys are the note's slug plus 12 hex digits of its identity, so reruns report `unchanged`, replace edited
notes, and refuse notes over their cap or holding a secret (never printed). `--dry-run` prints the plan and writes
nothing; without `--dry-run`, Hindsight must be configured. `ultron memory` without `import` stays a prompt.

## Working-tree checkpoints, file-restoring rewind and request traces (wave 38, w14-checkpoint)

Ideas from reflection-oss/mirror (`mirror-agent/src/checkpoints.rs`, `http/traced_model.rs`, the CLI's rewind
picker). None of them is a model capability: the model's only tool stays `rlm`; checkpoints are a host guarantee
of the session, the restore is a user action, the trace is an operator's eval aid. Nothing is added to the model's
context.

### Checkpoints

- At each root user turn, before the turn runs, the session records the working tree as an unreferenced Git commit
  (`crates/delegation/src/checkpoints.rs`): a copy of the index, `git add -A` into it (assume-unchanged and
  skip-worktree flags cleared first), `write-tree`, `commit-tree` on top of HEAD (HEAD itself when the tree is
  HEAD's). The same method a worktree child's base snapshot uses; HEAD, branches, the real index and the files are
  untouched. Nothing references the commits, so Git may prune them after `gc.pruneExpire` (two weeks by default).
- In a Jujutsu repository (`.jj` above the working directory) with `jj` on the session's PATH (never the process
  PATH), jj's own snapshot is recorded (`--no-integrate-operation`).
- Stored in the session journal as one value per checkpoint in the `ultron.checkpoint` namespace, keyed by the
  prompt entry id (`crates/session-journal/src/checkpoints.rs`): `{kind: "turn", entryId, vcs, root, head, commit,
  tree, excluded, createdAt}`. A TypeScript host keeps such values as opaque.
- Cheap: a tree equal to the last checkpoint's records that checkpoint again (`unchanged: true`), no new commit.
  Bounded: more than `checkpoints.maxFiles` (100000) listed files records `skipped` with a note instead; untracked
  files larger than `checkpoints.maxFileBytes` (16 MiB) are left out (`excluded`).
- Settings: `checkpoints.enabled` (default true; only Git or jj working trees record anything). Off while the
  shell sandbox is on (`sandbox.mode` other than `off`): the checkpoint's Git runs on the host and would obey
  hooks, filters and fsmonitor commands that sandboxed code can write into the repository's config.

### Restoring files

- Session op `restoreFiles {entryId | checkpoint}` (`crates/session-core/src/core/checkpoints.rs`): the checkpoint
  attached to the entry, else to the nearest entry above it (the start of its turn). The current files are recorded
  first as a `restore:<id>` checkpoint attached to the branch tip, so restoring the tip again undoes the restore.
  Files the target lacks are deleted (empty directories pruned), the others written from it. Refused while a run
  streams, in the sandbox, and when it would overwrite a file the safety checkpoint cannot hold (ignored or too
  large); nothing is changed then. Ignored files, HEAD and the index are never changed.
- `checkpoints` lists them (`{checkpoints, enabled}`).
- TUI: `/tree` Shift+R (`app.tree.restoreFiles`; the help shows it only when an entry has a checkpoint) and the
  Esc-Esc rewind selector's `r` (`app.rewind.restoreFiles`) ask for confirmation; the rewind selector also offers
  "Restore files and rewind the conversation", which forks only after the files were restored.

### Request traces

- `ULTRON_REQUEST_TRACE=<file>` appends one JSON line per answered conversation request (the root lane `main` and
  each subagent conversation `conversation:<id>`) with the delta from the previous request of the same lane:
  `start`, `append` (`base` kept messages, the added `messages`) or `reset` with `reason` (`model`, `system`,
  `tools`, `compaction`, `branch_summary`, `context_edit` for `ctx.forget` and other context edits, `truncated`,
  `prefix_changed`) and `at`; plus `usage` and `stopReason`. Mode 0600, opened for appending.
- `ultron trace check <file>` prints each reset with its reason and a summary; exit 0 when every request appended,
  1 with a reset, 2 for bad input.

## Bash result conventions and the prompt budget check (w14-kernel)

The model still has one tool, `rlm`; these are host guarantees on results the kernel API already returns, and one
run guard.

- **Non-error exit 1** (`crates/host/src/bash_notes.rs`). `bash(cmd)` of a simple pipeline (commands joined by `|`
  or `|&` only) whose last command is `grep`, `egrep`, `fgrep`, `rg`, `diff`, `cmp`, `test` or `[` and that exits
  with 1 gets `exit_note` from the host ("No matches found", "Files differ", "Condition is false"). runtime.py prints
  `[No matches found (exit code 1)]` instead of the failure line `[exit code 1]`; `exit_code` stays 1 and `.ok`
  (a clean exit 0) stays False. Anything else (`;`, `&&`, `||`, `&`, newlines, subshells, `$(...)`, backticks,
  exit codes other than 1) keeps `[exit code N]`. Shell jobs (`yield_after=`) carry the note in their status line.
- **Sandbox refusal note** (`crates/sandbox/src/refusal.rs`). A failed command run under a sandbox (bwrap,
  unshare, docker) whose output shows a refusal the sandbox caused gets one `sandbox_note` line, printed after the
  output: a write to a path the policy keeps read-only that this (unsandboxed) process could write ("writes outside
  the project are blocked by the sandbox. Writable: <project>, ..., /tmp"), a path inside a hidden credential path
  that exists on the host, or a connection or name lookup failure while `sandbox.network` is off. Errors the
  sandbox did not cause (a path read-only on the host too, a missing file, `/tmp`) get no note.
- **Prompt budget** (`crates/compaction/src/prompt_budget.rs`, checked in the session actor before every root
  request). If the system prompt (context files, runtime and extension guides) plus the `rlm` tool schema, sized
  with auto-compaction's estimator, leave the conversation less than `compaction.reserveTokens` below the
  compaction threshold (`contextWindow - reserveTokens`; on windows under three times the reserve the fixed part may
  take up to `reserveTokens`, so small-window models that run today keep running), the run fails before
  any provider call with "Prompt budget exceeded for <model>: the system prompt (N tokens) and tool schemas (M
  tokens) take ... Shorten the system prompt ..., lower compaction.reserveTokens, or switch to a model with a
  larger context window." With auto-compaction off only the window limits.

`exit_note` and `sandbox_note` are fields only the Rust host sends; runtime.py under the TypeScript host is
unchanged.

## `ultron acp`: Agent Client Protocol agent (w14-acp)

`ultron acp [session options]` serves ACP v1 (JSON-RPC 2.0, one message per line on stdio) so editors that speak ACP
(Zed, CodeCompanion.nvim, avante.nvim) drive Ultron. Setup for Neovim: [acp-neovim.md](acp-neovim.md).

- Same path as `--mode rpc`: each working directory a client opens sessions in gets one `rpc::remote::Remote` (this
  process's server generation, attached to the session worker); ACP requests are `RpcSession` operations, and the RPC
  event lines it writes are translated (`crates/cli/src/acp/translate.rs`). No logic is forked.
- RLM-first: the model still has one tool, `rlm`. Each cell is reported as one ACP tool call (`kind: "execute"`, the
  first code line as title, the code as content and `rawInput.code`, the output when it ends). Files a cell changed
  (a metadata picture of the session directory before and after, the worker's own snapshot code) become `locations`,
  `kind: "edit"` and `diff` content where the text before is known.
- Methods: `initialize` (loadSession, image + embedded-context prompts, `sessionCapabilities.list`, no auth methods),
  `authenticate` (no-op), `session/new`, `session/load` (replay as `session/update`s before the answer),
  `session/list`, `session/prompt` (stop reasons `end_turn`, `cancelled`, `max_tokens`, `refusal`; other failures are
  JSON-RPC errors), `session/cancel`, `session/set_config_option` (`model`, `thought_level`). `usage_update` after
  each turn, `session_info_update` on rename.
- `session/request_permission` only where Ultron already asks: an extension's `confirm`/`select` dialog during a turn.
  No per-tool approval.
- Not used: the client's `fs`/`terminal` capabilities (the kernel does its own IO), MCP servers passed in
  `session/new` (the session's MCP gateway reads `mcp.json`; a note goes to stderr), session modes.

## A stuck cell never locks rlm (w15-lanes)

A cell that hangs (a `git push` waiting forever) used to hold its REPL lane, and every later `rlm` call was refused
"REPL lane main is still running cell-N". Now:

- A busy lane never refuses a call. The cell runs in a side lane of it (`main-side-1`, its own kernel; its host
  requests are main's: shell jobs, `ctx`, budget, events), and its result starts with one line:
  `[lane main is still running cell-N (started Ns ago); this ran in lane main-side-1; await rlm.cancel("cell-N") stops it]`.
  The stuck cell is detached: its own call returns "still running in the background", and its end arrives as a
  `cell_done` runtime event at the top of a later result. Once main is free, calls go back to it and idle side lanes
  close. A cell being cancelled is waited for (within the cancel grace) instead of being routed around.
- `await rlm.cancel(cell_id)` (host request `rlm.cancel`, same as `rlm.cell.cancel`) cancels a running cell of any
  other lane: its host waits end, and past the grace its kernel is killed with every process it started.
  `rlm.cells()` marks a cell being cancelled with `cancelling: True`.
- `ultron mcp`: `notifications/cancelled` (Esc in Claude Code) cancels the call's cell through the same abort path as
  Esc in the native TUI, stops the shell jobs and REPL calls of the turn (`cancel_root`), and the call answers
  `Abort requested`; the lane is free for the next call.
- Same routing on the native root lane (it is the `rlm` tool's, crates/session-core `Rlm`).

## Host-checked delegation contracts (wave 40, w16-contracts; decision row 210, first half)

`rlm.spawn(brief, name=..., role=..., contract={...}, hop=...)` (and the same `role` / `contract` / `hop` fields on
`rlm-child@1` workflow nodes, which then run as subagents of the subagent host) gives a child a contract that the
HOST verifies itself when the child ends. Nothing is trusted from the child, and there is no approval or gate.

- `role`: a `Role: ...` line ahead of the brief; the brief also names the checks the host will run.
- `contract={"checks": [...], "returns": <JSON schema>}`, at most 16 checks:
  `{"run": "<cmd>", "expect": "exit 0" | {"exit": N} | {"stdout_contains": s}, "timeout_ms": N}` (default exit 0,
  120 s, at most 600 s; `/bin/sh -c` in the child's working directory: its worktree with `worktree=True`, else the
  session's, with its worktree environment, under the session's shell sandbox with that directory writable);
  `{"changed_files_within": [globs]}` (every file that changed in its working directory since it started, from the
  workspace snapshot the verdict check already takes); `{"no_new_dependencies": true}` (no manifest or lockfile
  changed: Cargo.toml/lock, package.json and npm/yarn/pnpm/bun locks, pyproject/poetry/uv/Pipfile/setup.py/cfg,
  requirements*.txt, go.mod/sum, Gemfile(.lock), composer.json/lock); `{"files_exist": [relative paths]}`.
  `returns` is checked on `rlm.finish(outputs=...)`: a mismatch rejects the finish (the child fixes it, as any
  rejected verdict), and a child that ends without a matching verdict fails it.
- Result: `check.contract` (under `check`, the free-form field of the task journal's result shape, which TS reads
  too) = `{"verdict": "verified" | "contract_failed", "checks": [{"check", "ok", "evidence", "exit_code", "stdout",
  "stderr", ...}], "failed": [...], "attempts", "previous"?, "hop"}`. Command output is kept to its last 2,000
  characters. A failed contract makes the result `status: "failed"` with `error: "Contract failed: <each failed
  check>"` (so `rlm.merge` skips it unless `include_failed=True`); the child's own verdict stays beside it. The
  completion event's tag gains `contract verified` / `contract contract_failed`.
- `hop`: `"report"` (default); `{"retry": N}` (1..3) re-runs the child (same task, worktree and start snapshot)
  with `[Contract] Attempt k did not pass the host's contract checks: ...` appended to its brief, on a verdict of
  its own; `{"review": "brief"}` spawns a sibling reviewer `<name>-review` whose brief is the review brief, the failed
  checks, the child's summary and declared files; its handle is in `check.contract.hop.review`.
- Not kept across a worker restart: a child reattached by the durable runtime ends with its ordinary verdict check.
- Python: `swarm_api.py` (Rust-served additions) passes the options through and documents them in
  `help(rlm.spawn)`; the TS host refuses them (`Unknown payload field: role`). The Rust runtime guide names them in
  the delegation section with the durable APIs on (`ULTRON_GUIDE=ts` keeps the TS text).

## Session environment for kernels and commands; cell cancel stops its shell work (w16-kernelfix)

Two bugs found by w15-lanes, fixed in crates/kernel, crates/host and crates/session-core `compose.rs`.

- A session's kernels and commands start from the session's environment, never the server process's. The stack
  builds it once per kernel or host (`compose::session_environment`): every variable name this process has plus a
  fixed list (PATH, HOME, locale, TMPDIR, XDG dirs, Python and `ULTRON_RLM_*` settings), each valued by the session's
  `var` lookup and dropped when the session has none. `KernelOptions::base_env` replaces `std::env::vars()` for the
  kernel (before the credential filter) and for its `ULTRON_PYTHON`, limit, output-budget and tree-memory settings;
  `Host::with_base_env` makes blocking bash and shell jobs `env_clear` to it (then the command's own `with_env`
  variables), finds bash and the job keeper on its PATH, and gives the sandbox's allowlist its values. A tool only
  the server's PATH has is not found. In the product the session's lookup is the worker's process environment, so
  nothing changes there; injected environments (tests, embedders) now reach the children. The sandbox launcher
  itself (bwrap, unshare, docker) is still found as the sandbox crate's mechanism probe finds it (`launch_env`).
- Cancelling a cell (`rlm.cancel`, `rlm.cell.cancel`, `Lanes::cancel`) stops its shell work at once. Each cell's shell requests
  (`bash`, `shell.bash`, `shell.run`, `shell.result`) run under a signal that also aborts on the cell's cancel: a
  blocking bash's own process group is killed, a waited-on job is cancelled, and every job the cell started is
  cancelled (`shell.cancel`). The request then fails with "the cell was cancelled; its shell command was stopped",
  so Python raises and the cell ends within the cancel grace: the kernel is not killed and the lane keeps its
  variables (no restore from the last checkpoint). Jobs of cells that ended normally keep running.
- Every host kill of a command goes through `kernel::kill_command_group(pgid)`: only the explicit positive group the
  host created for that command (`process_group(0)`), never 0, -1, 1, the host's own pid or its own group.

## `rlm.images`, written-file diffs in the TUI, no approvals or grants (wave 40, w16-cleanup)

**`rlm.images`.** `r = await rlm.images.generate(prompt, model=None, size=None, n=1, out=None)` generates images
through OpenRouter's images API (the port of `api/openrouter-images.ts`, `ai::images`) and returns
`{"paths": [...], "model": "openrouter/<id>"}`: the host saves every image to a file, so no image bytes reach the cell
or the model's context (`view_image(path)` shows one). `model` is `provider/id` or a bare catalog id; default: the
`images.model` setting, else `openrouter/google/gemini-2.5-flash-image`; `await rlm.images.models()` lists the
catalog. `size` is an aspect ratio (`"16:9"`) or `1K`/`2K`/`4K`, sent as `image_config`; `n` (1-4) makes requests
until that many images came back; `out` is the file (several images: `<stem>-1.<ext>`, ...) or an existing directory;
default `<agent dir>/rlm-output/<session id>/images/image-<k>.<ext>` (first unused numbers). The key is resolved as for
chat requests (auth.json, models.json, `OPENROUTER_API_KEY` of the session environment); `images.baseUrl` replaces
the provider URL (a proxy). Argument errors are `ValueError` in Python and `images.generate ...` messages from the
host; provider failures and answers without an image are `RuntimeError: Image generation failed: ...`, and nothing is
written unless every image arrived. The Python side (`images_api.py`, installed from `swarm_api.install` so
runtime.py's line numbers stay put) is ignored by the TypeScript host, which answers `images.*` with an error. The
ultron-rs runtime guide gets one line behind `durable_apis` (`ULTRON_GUIDE=ts` keeps the TS text).

**Diffs of the files a cell wrote.** Writes through the checked `write()`/`edit()` (`files.before_write`) are
recorded per lane with the text before (read while the write is proposed) and after (`host::cell_writes`, at most 32
files, 256 KiB per text). When the cell ends, those whose file now holds the proposed text become the result's
`details.fileChanges`: `[{path, created, diff, omitted?}]`, `diff` in the edit tool's display format
(`generateDiffString` with 2 lines of context, at most 400 lines; null for a file too large or not text). A cell that
wrote nothing keeps `details: {}` as in TypeScript. The interactive mode draws the list under the expanded rlm cell
(`N files changed`, the path, `renderDiff`'s colored lines with word-level inverse for a single changed line); the
tool-expand key toggles it and the collapsed cell is byte-identical to TypeScript's. Files changed by other means
(`bash('sed -i ...')`) are not in the list: their text before is unknown when the cell ends.

**No approvals, no grants.** The `approval.*` settings (omp's tiers, `approval.ops`, bash patterns and the critical
pattern guard), the `Approver` UI hook and `grants.*` (scoped approvals) are gone: every operation the sandbox
contains runs. `sandbox.*` (bwrap, unshare, docker, write containment) is unchanged. The shared runtime still binds
`grants` (the TypeScript product serves it); in ultron-rs its calls fail as unserved host requests.
