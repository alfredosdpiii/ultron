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
- Kept across a worker restart (see w17-env below); `changed_files_within` and `no_new_dependencies` compare with a kept git baseline after one (wave 41), else fail closed.
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

## Contracts survive a restart; every kernel and the sandbox launcher get the session env (w17-env)

**Kept contracts.** A subagent spawned with `role=` / `contract=` / `hop=` has its contract written to the session
journal as a session value, `ultron.subagent-contracts` / `<task id>` (the `SpawnContract::describe` form:
`{"role", "checks", "returns", "hop"}`), in the same commit as its name (`ultron.subagent-names`), before its child
conversation exists. Values are append-only journal writes replayed on open; the last write per task id wins. A
worker that reattaches an unfinished child (`DurableChildren::restore`) reads the value back
(`SpawnContract::from_described`, the spawn parser again) and the host holds the child's end to it as before the
restart: `returns` is checked on its `rlm.finish`, the checks run when it ends, a failure is `contract_failed`, a
`{"retry": N}` hop re-runs it (the run rebuilt from its child task), a `{"review": ...}` hop spawns the reviewer.
- Sessions from before have no value: their children are reattached and end with their ordinary verdict, unchanged.
- A kept value that no longer parses fails the child: `Contract failed: the session's kept contract is unreadable:
  <error>` (never treated as no contract).
- `changed_files_within` / `no_new_dependencies` need the start snapshot, which is not kept: after a restart they
  fail with "no working directory was snapshotted when the child started" (fail closed). `run`, `files_exist` and
  `returns` work as before. `keep=True` takes contracts since wave 41 (see below).

**Session env for code-skill and review kernels.** `skills.propose_code` / `propose_policy` test kernels
(`CodeSkillsOptions::for_session`, `base_env` = the session's `compose::session_environment`, and the session's
`ULTRON_CODE_SKILL_TEST_TIMEOUT_MS`) and the review engine's kernels (`EngineOptions::base_env`, set by `ultron
autoreview` from its `Env`) start from the session environment, as the lanes' kernels do: a tool only the server
process's PATH has is not found.

**Sandbox launcher.** `Policy` carries `launch` (`launch_env(host_env)`: the session's PATH, else
`/usr/local/bin:/usr/bin:/bin`, and its DOCKER_HOST; never the process's). The launcher binary (bwrap, unshare,
setpriv, the Docker client's bash) is resolved on that PATH (`resolve_program`), `setpriv` is used only when that
PATH has it (`Policy::layout`), and the launcher process starts with exactly that environment, for the mechanism
probe, shell commands, persistent jobs, contract checks, kernels (`crates/kernel/src/sandboxed.rs`) and `ultron sandbox
doctor` (which now builds its policy from the session environment). `RealLayout::which` looks on the fixed PATH only.

## Ultron persona, auto-nudge, proactive skills (w17-persona)

**Persona.** `"persona": "ultron"` in settings.json (agent dir; a trusted project's settings may override it like
any key) puts the text of `<agent dir>/personality.md` in the root system prompt as its own `<persona>` section,
after the context files and before the skills. Any other value, or none, leaves the prompt unchanged. A missing or
unreadable file is one warning (`persona: cannot read ...`) and no section. The file is read when the prompt renders:
at session start and when the settings' prompt mode changes (engineering, `alwaysSkills`, persona), never per
request, so the prompt-cache prefix stays stable. `/persona` reports the setting, `/persona ultron|off` writes it
(off removes the key); RPC clients set it through the worker's `setSetting` (`persona`: `ultron`, `off` or null,
applied live). `ultron claude` gets the same section in Claude Code's system prompt (root only, never a subagent),
followed by `The pstack skills directory is <bundled>/pstack (principle-<name>/SKILL.md).`; nothing is installed in
~/.claude.

**Auto-nudge** (persona on only). After a root run ends normally (not aborted, no error, no queued user message or
other waiting input), the host starts a new run with the nudge when the run made at least one rlm call, the final
assistant text's last paragraph (split on blank lines) does not end with `?` and matches
`\b(next,? I(?:'ll| will)|I(?:'ll| will) (?:now|next|then|start|run|add|fix|write|check)|then I(?:'ll| will)|let me|next step|going to|about to)\b`
(case-insensitive), and fewer than 5 nudges ran since the user's last input. The nudge text is
"Continue: take that next step now. End the turn only when the work is done and verified, or when you need the user;
then ask one direct question." It is a displayed custom message (`customType: ultron.nudge`,
`details: {nudge, max}`), journaled as the run's prompt, which the model reads as a user message; the TUI draws it as
one muted line `auto-continue (n/5)`, live and on resume. A prompt or queued message from the user resets the count;
an abort ends the chain until the next input. The rule and text are those of the user's Claude Code hook
(ultron-persona.py), recorded in crates/resources/fixtures/persona-nudge.json. For `ultron claude`, the server's
answer to the Stop hook the launcher already installs applies the same rule to Claude Code's transcript (its tool
calls count as the rlm calls) and returns `{"decision": "block", "reason": <nudge>}`; UserPromptSubmit resets the
count; `stop_hook_active` does not stop the chain, the cap does.

**Proactive skills in engineering mode.** With engineering mode on and the Rust guide (not `ULTRON_GUIDE=ts`), the
bundled-skills note says "Use them on your own: when a task fits one, read it and follow it without being asked, and
say which in one line." instead of "Read one only when the user asks for it ...". `ultron claude` launches with the
same wording when engineering is on; `ultron guide --for claude` keeps the TypeScript wording it is recorded with.

## Terminal background detection (OSC 11) (wave 41, w18-osc)

At TUI start without a `theme` setting, and whenever an automatic `light/dark` pair is applied, the app sends the
OSC 11 background query `ESC ] 11 ; ? BEL` (`queryTerminalBackgroundColor`). The theme first comes from `COLORFGBG`
(else the dark fallback) and is drawn at once; nothing waits for the answer. The reply
(`ESC ] 11 ; rgb:R/G/B` or `#RRGGBB` / `#RRRRGGGGBBBB`, ended by BEL or ST, any number of hex digits per channel, as
`parseOsc11BackgroundColor`) is taken out of the input wherever it arrives (`crates/tui/src/terminal_colors.rs`,
`Osc11Reader`): a complete reply is never typed text; while a query is outstanding a reply cut short by the stdin
buffer's timeout is held and joined with what follows (a held fragment that already began `ESC ] 11 ;` and stops
looking like a reply is dropped; a shorter one, which may be Alt+], is handed on as keys). When the reply arrives the
reported background wins (`getThemeForRgbColor`: relative luminance >= 0.5 is light): without a setting that theme
applies with a repaint and is saved as the setting (the detection is confident); with a pair its side applies unless a
`CSI ? 997 ; 1|2 n` scheme report came first (`detectTerminalThemeForAuto`: the scheme wins). A reply after
`/settings` chose a named theme, an unasked second reply, or an unparseable one changes nothing. Without a reply the
`COLORFGBG` result stands.

## Contract follow-ups: Git baselines, contracts for kept children, guide wording, persona polish (wave 41, w18-small)

**Workspace checks after a restart.** A contract with `changed_files_within` or `no_new_dependencies` now keeps a
Git baseline with its session value (`ultron.subagent-contracts` / `<task id>`, key `baseline`) when the child's
working directory is in a Git work tree with a commit: `{"commit": HEAD, "status": <blob id of git status
--porcelain -z -uall --no-renames -- .>, "dirty": {<path>: <blob id or "-">}}` (at most 256 dirty paths, else none).
A host that reattaches the child after a restart compares with it (`delegation::git_baseline`): changed =
`git diff --name-only --relative <commit>` plus untracked files (`ls-files --others --exclude-standard`), minus paths
dirty at the start whose content is unchanged, plus dirty ones the child restored; a same-commit, unchanged-status,
clean tree short-cuts to nothing. Git runs in the working directory with the session's environment
(`Delegation::set_command_env`, compose's `session_environment`; GIT_DIR/GIT_WORK_TREE and similar redirects
dropped, `core.fsmonitor=false`, never a remote; no PATH: `/usr/local/bin:/usr/bin:/bin`). The live path keeps its
metadata snapshot. Outside Git (or before the first commit, or git missing) no baseline is kept and the checks fail
closed after a restart with "no working directory was snapshotted when the child started, so its changes are
unknown", as before. Contracts without workspace checks are kept exactly as before (no `baseline` key).

**Contracts with keep=True.** `rlm.spawn(brief, name=..., keep=True, role=, contract=, hop=)` is accepted
(swarm_api.py no longer refuses it; the TS host still rejects `keep`). The session's swarm ops
(`crate::kept_contracts`) parse the contract as a one-shot spawn does, lead the brief with the role and checks, and
keep `{"contract", "baseline", "verdict", "first"}` as `ultron.kept-contracts` / `<child id>`. The kept child's
`rlm.finish` is served there (validated like a subagent verdict and against `returns`; rejections say "Fix these and
call rlm.finish again."); each `rlm.ask` starts it on a fresh verdict. Each answer is checked through the same
`spawn_contract::enforce` as a one-shot child's end: the first answer once, when `rlm.collect` / `rlm.results`
first reports it (that checked result is kept and reported from then on), each `rlm.ask` answer when it arrives.
`{"retry": N}` asks the child again with the failures (`[Contract] Attempt k ...`), `{"review": brief}` spawns the
reviewer subagent `<name>-review` from the asking lane. Workspace checks are cumulative since the spawn: the
in-process snapshot, else (after a restart) the kept Git baseline. Results carry `verdict` (that answer's own) and
`check.contract` as one-shot results do.

**`ultron guide --for claude`.** Prints what `ultron claude` launches with: with engineering on and the Rust guide,
the proactive bundled-skills note; `ULTRON_GUIDE=ts` keeps the TypeScript text (the recorded `guide-*` parity cases
run under it).

**Persona polish.** `/settings` lists `Persona` (ultron|off; off removes the key, as `/persona off`) outside the
TypeScript-parity TUI build (`ULTRON_TUI_PARITY=1` shows exactly TypeScript's rows). `/session` shows
`Auto-continues: N` under the tool counts when the transcript has N > 0 persona nudges.

**Sandbox environment.** A sandboxed host without a base environment builds its policy from an empty environment
plus `PATH=/usr/local/bin:/usr/bin:/bin` (`host::sandboxed::sandbox_host_env`), never the process's.

## Scheduled prompts: `schedule.every` / `schedule.at` (wave 41, w18-sched)

The proactive agent: a session sends itself a prompt later. RLM-first: no model tool; `schedule` is a kernel API
(`schedule_api.py`, installed from `swarm_api.install`; the TypeScript host answers its requests with an error), the
server owns timing, and the user sees every schedule (`/schedules`, `ultron schedule list`). No approvals.

- **API** (root agent only; subagents get "schedule.every is available to the root agent only"):
  `id = await schedule.every(interval, prompt, *, name=None, until=None, max_runs=None)` (interval `"90s"`, `"15m"`,
  `"1h"`, `"1d"`, `"1h30m"`, a number of seconds or a timedelta; at least 60 s, at most 365 d; the first run one
  interval from now), `id = await schedule.at(when, prompt, name=None)` (ISO time, naive times local, a datetime, or
  seconds from now; must be in the future), `await schedule.list()`, `await schedule.cancel(name_or_id)`. Names are
  one word, unique among active schedules (default: the id `sch-<8 hex>`); at most 32 active schedules per session;
  prompts up to 8 KiB. The Rust guide gets one pointer line (`help(schedule)`); `ULTRON_GUIDE=ts` has none.
- **Storage**: one session value per schedule, `ultron.schedule` / `<id>` (`crates/kernel-apis/src/prompt_schedule.rs`):
  `{id, name, sessionId, prompt, kind, intervalMs, until, maxRuns, createdAt, nextDue, runs, lastFired, state}`,
  append-only journal writes replayed on open; a run, a cancel and the end are writes too. A value whose `sessionId`
  is not the session's own (copied by a tree fork) is ignored, so a fork never fires its source's schedules.
- **Firing** (`crates/daemon/src/prompt_schedules.rs`): each server generation reads its store's journals when it
  starts (complete lines only, never opening the session), then keeps what workers report after every change
  (`worker_schedules`). One timer thread sleeps until the earliest due time (wall clock, at most 5 minutes per sleep,
  no polling). A due schedule is delivered through the hub as the session operation `scheduleFire {id, due}`: the
  worker is started or adopted like a client attach (attached clients see the run live), and the session actor
  records the run and sends the prompt as a displayed custom message (`customType: ultron.schedule`,
  `details: {name, id, run}`) that starts a run; during a run it waits for that run's end (never interrupting).
  A fire names its slot: a duplicate (another generation) or early fire does nothing. Missed slots collapse into one
  fire (the next slot is the first after now). A session whose file is gone is dropped; a delivery that failed is
  retried after 60 s. Schedules fire while an Ultron server runs (an interactive `ultron`, `ultron durable`, the
  `ultron service` unit); a server holding a pending schedule never retires on idle, and a server that starts later
  fires what it missed once (wave 43: see "Durable service" below).
- **Visibility**: `/schedules` (with the overlays) lists `name  next <ISO>  runs n[/max]  every 15m|once  <id>`; the
  transcript shows a fired prompt as one muted line `scheduled: <name> (run n)`, live and on resume.
  `ultron schedule list` prints `ID NAME NEXT RUNS REPEAT STATE SESSION CWD` (tab-separated) from the journals;
  `ultron schedule cancel <id|name>` goes through a running server of the instance (server service
  `ultron.schedules`, unlisted), else writes the journal under the session's locks. Any other `ultron schedule ...`
  stays a prompt.
- Tests use a clock file (`__ULTRON_SCHEDULE_CLOCK_FILE`, epoch ms; the timer looks at it every 20 ms), never real
  waits.

## Mermaid diagrams: `diagram.mermaid` (wave 42, W19M-)

The model renders Mermaid through the REPL; the user sees the diagram inline, the model only its path.

- **API** (`packages/coding-agent/src/ultron/rlm/diagram_api.py`, installed by swarm_api.py like `schedule`; the
  TypeScript host answers the request with an error): `d = await diagram.mermaid(text, *, theme="auto", show=True,
  to_model=False, path=None)` returns `{svg_path, png_path, width, height, type}` (PNG pixels, Pele's diagram type).
  The Rust guide gets one pointer line (`help(diagram)`); `ULTRON_GUIDE=ts` has none.
- **Rendering** (`crates/kernel-apis/src/diagram.rs`): Pele 1.7.0 (MIT, `crates/kernel-apis/assets/pele/`, with
  `LICENSE` and a `VERSION` note carrying the sha256) is embedded with `include_bytes!` and unpacked once to
  `<agent dir>/cache/pele-<sha256 prefix of pele+runner>/` (staging directory renamed into place; a damaged one is
  replaced). The host runs `node runner.mjs` with node found on the **session environment's** `PATH` only (never the
  process's), an environment of just that `PATH` and the session `HOME`, the text on stdin, 20 s timeout (the
  child is killed and reaped), 16 MiB output cap, cancelled with the cell. No node: `diagram.mermaid needs Node.js
  on PATH; the source stays in the transcript`. A Mermaid error is Pele's message (plus `(line L, column C)` when
  the message does not name the line), raised as RuntimeError in the cell.
- **Theme** (`crates/kernel-apis/src/diagram_svg.rs`): every `var(--name, fallback)` inside tags is replaced
  (`--pele-*` by the theme's colour, `--_*` by the SVG's own definition, else the fallback, else the theme's
  foreground; nested and cyclic references handled), `color-mix(in srgb, ...)` of the results is computed, the
  custom-property declarations are dropped; labels between tags are never touched. Colours come from the TUI theme
  (`tui::theme`): bg = export.pageBg, fg/line = text, muted, surface = toolPendingBg, surface-alt = selectedBg,
  border, accent, critical/error = error, series 1-8 = accent, success, warning, error, border, thinkingHigh,
  mdHeading, syntaxType. `theme="auto"` reads the `theme` setting at each call (a `light/dark` pair or unset follows
  the session's `COLORFGBG`, else dark; a custom theme from `<agent dir>/themes`); `"dark"`/`"light"` override.
- **Files**: the flattened SVG and a PNG drawn by resvg 0.48.1 at 2x on the page colour (longest side capped at
  8192 px; system fonts with sans/serif/mono pointed at installed families) go to
  `<agent dir>/rlm-output/<session>/diagrams/<slug>-<n>.svg|png` (slug from the diagram's title, else its type;
  first free n), or `path=` (a stem; `.svg`/`.png` dropped; an existing directory takes the default names).
- **Showing**: `show=True, to_model=False` (default) keeps the PNG for the user only: the session's cell images
  (`session_core::view_images::ViewImages::display`) put it in the `rlm` result's `details.displayImages` (up to
  4 MiB; providers never see details) and the model's text gets one line `diagram: <png path> (WxH)`; the TUI
  (`interactive::chat`) draws details images after the result's content, live and on resume. `to_model=True`
  attaches the PNG like `view_image` (model-normalized image block). `show=False` only saves.
- Tests: a fake `node` on the injected session PATH; the real node with the vendored Pele runs only with
  `ULTRON_MERMAID_TEST_NODE=1`.

## Durable service, `ultron-s`, schedule delivery, usage, contracts, diagram contrast (wave 43, W21*)

**Service** (`crates/cli/src/service_cli.rs`). `ultron service install|uninstall|status|restart|run` manages a systemd
user unit for the **durable** instance only (`~/.ultron/server`): `ExecStart=ultron service run`, `Restart=always`,
`WantedBy=default.target`, working directory `$HOME`, `EnvironmentFile=-<agent dir>/service.env`, no keys in the
unit. `systemctl` and the unit directory come only from the injected environment; no systemd prints "No systemd
found" (exit 1) and writes nothing, and a failed enable removes the unit. Project instances are unchanged: a plain
`ultron`, `ultron --claude` and `ultron claude` keep their own foreground servers.

**Durable clients** (`crates/cli/src/durable_client.rs`). `ultron durable` (interactive, `-p`, `--mode json`)
attaches to the durable server, which outlives its clients; per-run worker options (`--model`, `--system-prompt`)
are refused, since one server serves one worker model. Durable sessions get the persona by default through a process
settings default `{persona: "ultron"}` (worker option `settingsDefaults`, below the settings files, never written);
`/persona off` there is saved as an explicit `"off"`. An idle durable server without schedules makes no provider
requests and starts no worker: it does nothing unless instructed.

**`ultron-s`** (argv0 dispatch; `install.sh` links `ultron-s -> ultron`). Attaches to the durable instance's home
session, recorded in `<durable dir>/home-session`: created once in `$HOME`, reused across clients and server
restarts, replaced when deleted. `--new` replaces it, `--list` lists, `--session <id>` / `--home` pick one, `-p`
prompts it. A client killed mid-run leaves the run going; the next client continues after it. `install.sh` runs
`ultron service restart` when an `ultron-durable-*.service` unit is installed.

**Schedule delivery.** A fire counts only once its prompt started or queued; a failed delivery keeps the slot due
(an `at` schedule stays active), records `{error, at, due}` in `schedule-errors.json` and shows in the `ERROR` column of
`ultron schedule list`. `schedule list` wakes the instance when a schedule is active.

**Usage** (`crates/cli/src/usage/log.rs`). `ultron usage` reads durable children's responses from the session's
durable records (`ultron.module/durable` `pi.assistant` entries): model, input/output/cache split and cost per
response. A child on the root's model merges into its row; another model gets its own row. Ledger turns that the
records lack stay a totals-only row, so totals equal the ledger; a conversation with no named owner goes to "other".

**Contracts.** A Git baseline is HEAD plus a tree id written through a temporary `GIT_INDEX_FILE` (a copy of the
index in a private temp dir; the real index is never written), so any number of dirty paths fits; older
`{commit,status,dirty}` baselines still compare as before; a pruned tree fails closed. Kept children: failed check
runs share one budget across answers (final when failures reach retries + 1, persisted in `ultron.kept-contracts`
across restarts); verified answers cost nothing; rejected finishes are bounded per answer.

**Diagrams and terminal theme.** Labels are recoloured after flattening to reach WCAG 4.5:1 against what is under
them (text colour, then page colour, then black or white; pie percentages against their own slice), checked for all
23 Pele kinds in both themes. The TUI's detected appearance (OSC 11, `COLORFGBG`, scheme report) reaches the session
as `setTerminalTheme`, replayed on every attach; `theme="auto"` prefers it over the worker's `COLORFGBG`.

**Server.** A client attaching while the idle worker retires waits for it to exit and starts a new worker (the
`radius_client` flake). `--help` lists every dispatched subcommand.

## Per-session persona and keep-going mode (wave 43, W22K-)

**Toggle.** Each session has its own persona state, the session value `ultron.module/persona`: `on`, `off` or `auto`
(follow the `persona` setting). It survives a restart and is read by `session-core` and by `ultron claude`'s hooks
(same session file). TUI: `/persona on|off|auto` sets it, `/persona ultron|none` writes the global setting, bare
`/persona` reports as before; `/keepgoing [on|off]`. `-p`/rpc take the same slash prompts and the rpc commands
`persona` and `keep_going` (none starts a turn). In `ultron claude` the prompt hook answers them by blocking the
prompt with the new status, and a toggle that differs from the launch prompt's is told to the model once.

**Phrase trigger** (`crates/resources/src/keep_going.rs`, cases in `crates/resources/fixtures/keep-going-phrases.json`).
"keep going", "don't stop until you finish", "I'm going to sleep", "work through the night" and similar, addressed
to the agent, turn the persona on and start keep-going, with a one-line `keep-going on: ...` notice. "stop the
server", "don't stop the process" and "the loop keeps going forever" do not match.

**Keep-going.** After a turn that did work (an `rlm` call; in `ultron claude`, growth of the transcript's `tool_use`
count) the agent is told to continue, with no 5-nudge cap; a turn ending in a question gets "the user is away:
decide, record the choice and the alternative, keep going". It ends on a turn with no `rlm` call (the final
report), user input without the phrase, an abort or failure, `/keepgoing off`, `/persona off`, or the ceiling of 100
continues or 8 hours (one final nudge asks for the report; `ULTRON_KEEPGOING_MAX_CONTINUES` / `ULTRON_KEEPGOING_MAX_MS`
override it). Mode and counters persist in the session, so a restart resumes them. Guardrails
(`resources::persona::GUARDRAILS`) are appended to the persona section and repeated in every nudge: irreversible or
outward actions are skipped and listed in the final report, and schedules start only when asked.

## herdr agents panel (wave 43, W23H-)

Ultron reports to herdr's socket when it runs in a herdr pane (`HERDR_ENV=1`, `HERDR_SOCKET_PATH`, `HERDR_PANE_ID`,
read from the injected environment only); herdr-gpui shows the same entry. User doc: [docs/herdr.md](../herdr.md).
The `crates/herdr` reporter is a background thread with a latest-state-wins queue, 0.5 s connect/write/read
timeouts (connect through `SO_SNDTIMEO`), silent errors, idle/blocked held 300 ms so continuations stay `working`,
and a release on drop, quit or signal. TUI: session, state (blocked when the answer ends with a question or the run
failed on auth, model or usage limits), metadata "Ultron" with title, `model`, `ctx`, `cost`; resume command
`ultron --session <id>` (`ultron-s` / `ultron-s --session <id>` for durable sessions). `-p`/json/rpc: state only.
`ultron claude`: "Ultron (Claude)" from Ultron's hook server, resume `ultron claude --resume <id>`, Notification and
SessionEnd hooks only inside herdr. Off with `"herdr": false` or `ULTRON_HERDR=0`; never from inside a kernel.
`herdr agent start --kind` accepts only herdr's built-in kinds, so Ultron panes are started as plain commands.


## `ultron claude`: always-on skills and waking the idle root (wave 43, W24-)

**Always-on skills.** The launch prompt applies `alwaysSkills` as native Ultron does (`claude-root/src/guide.rs`
`load_prompt_resources`): in engineering mode their text is in the prompt; otherwise they are listed only.

**Waking.** Claude Code cannot be resumed by the MCP server after the model replied, so the interactive root is woken
through a Claude Code `asyncRewake` hook: after every `rlm` call a `PostToolUse` hook (`ultron hook wait-events`,
async, 24 h timeout) waits in the background on the hook server's `wait-events` control op. It returns when the
root's turn has ended with runtime events waiting (the hook prints them to stderr and exits 2, which wakes the model
with them), when nothing the root started is running (subagents, tasks, shell jobs, tool calls, events in the
coalescing window) for 2 s, or when a newer waiter replaces it. `rlm.collect` never holds the turn. With the wake on
(`--wake` to `ultron mcp`), the root's guide uses native wording ("end your turn; each end arrives as a `child_done`
event") and the wait nudge drops "nothing wakes you". `-p`, `--no-hooks` and `ULTRON_GUIDE=ts` keep the next-call
guide and add no hook (Claude Code ends async hooks with a print run).

## Wave 44: wait rules, `ultron claude` parity, entry points (W25W-, W25C-, W25E-)

**One wake rule** (`session_core::wake`). `rlm.collect` leaves waiting to the host only on lane `main` of a root that
something wakes: native with completion events on, or `ultron mcp --wake` (the interactive `ultron claude` root).
Everywhere else (child lanes, `ULTRON_ASYNC_EVENTS=off`, `-p`, `--no-hooks`, Claude children, in-memory children) it
waits in the cell, unparked and cancelled with the cell; `timeout_ms` bounds the wait and raises with the ids still
running. `-p` and json runs stay while a typed task or detached cell they started runs. Guides, cell hints, the wait
nudge and the detached-cell note follow the same rule; subagents get no `schedule` line and in-memory children no
persistent-agent section. The `rlm.ask` timeout names `rlm.delete_subagent`.

**`ultron claude` parity.** Claude children are charged like TS `externalChild` (a model reservation on the spawning
turn's root, settled from the child's usage; a spent root admits no child; `ultron usage` sees them). `rlm.childModel`
applies to Claude children only when it is a `claude-code/` model; a named frame model beats `rlm.frameModel`.
Event summaries use the session's mask. The guide comes from the native MCP guide (servers, typed memory, images,
diagrams, contracts), and what the external root cannot serve (`schedule.every/at`, `keep=True`, `rlm.ask`, swarm,
board, notes) is refused with a clear error. Loki's policy reaches the root and Claude children; SYSTEM.md and
APPEND_SYSTEM.md shape root and child prompts; each turn records a working-tree checkpoint. A native `keep=True`
spawn refuses options it cannot honour.

**Entry points.** Durable clients attach under `--offline` and `httpProxy` (the server runs with the client's
environment; real conflicts are named). `/goal`, `/persona` and `/keepgoing` are answered by the worker in `-p`, json
and ACP. Durable `/persona` reports the worker's state and `/persona none` sticks. `--skill`, `-a`/`-na`,
`--use-theme`, `--verbose`, `--tui-mode` and `--models` (Ctrl+P) apply; flags still unapplied warn and left the help.
`--thinking` applies on resume without `--model` (TS keeps the stored lane with `--model`). `ultron-s` resolves
`@file` in the caller's directory and refuses `-c`/`-r`/`--fork`; ACP refuses session selectors and names only its
first session; `--no-session --name` keeps the name; herdr resumes a durable pane through `ultron durable --session`.
