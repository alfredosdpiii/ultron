# Working on Ultron

Ultron is a fork of Pi (`badlogic/pi-mono`): a terminal coding agent whose model has one tool, a persistent Python
REPL. Published as `ultron-agent` on npm and at github.com/alfredosdpiii/ultron.

## Style

- Short, direct, technical. No emojis, no filler.
- Answer a question before editing. When responding to feedback, say whether you agree before what you changed.
- Explain a non-trivial design as: problem, a concrete example or short trace, then the fix and why it is needed.

## Done means

A change is done when it works and is verified, not when it is written:

- Code changes: `npm run check` is clean (full output; fix errors, warnings and infos), and the tests you touched or
  that cover the change pass.
- Runtime, host, kernel, prompt or TUI changes: also `./test.sh` (all non-e2e tests) and `npm run test:acceptance`
  (rows A01-A56 must all pass).
- Before a release: the full list in [.pi/skills/release.md](.pi/skills/release.md), including
  `npm run scan:secrets`.

You may run builds (`npm run build:offline`), checks and tests, fix failures and rerun, without asking. Keep going
until the change meets the bar above or something genuinely needs the user; don't end a turn by announcing the next
step instead of taking it.

## Tests

- `./test.sh` from the repo root for non-e2e tests. Don't run a package's full vitest suite directly: e2e tests
  activate when provider keys are in the environment (the shared setup strips them unless `ULTRON_LIVE_TESTS=1`).
- A single file: `node "$(git rev-parse --show-toplevel)/node_modules/vitest/dist/cli.js" --run test/x.test.ts`
  from the package root; `packages/tui` uses `node --test test/x.test.ts`.
- `packages/coding-agent/test/suite/`: use `test/suite/harness.ts` and the faux provider; no real providers or keys.
- If you change an acceptance evidence test, `test:acceptance` reports its rows as unverified: review the change,
  then `npm run acceptance:lock`.
- Tests that need Python use the kernel's interpreter (`ULTRON_PYTHON`, else `/usr/bin/python3`), not bare `python3`.
- Add the GitHub issue number next to a regression test for an issue.

## Live runs and evals

- Any live `ultron` run you start (smoke tests, demos, evals) sets `ULTRON_SERVER_DIR` and `ULTRON_CODING_AGENT_DIR`
  to private temp dirs. Never use the defaults: the user has live sessions.
- Never leave copies of `auth.json` or keys behind; delete temp agent dirs after use.
- `scripts/eval-quality.mjs` makes paid model calls; unknown flags exit 2 (no `--help`). Record results under
  `acceptance/quality/`, with no home-directory paths.
- The runtime guide (`src/ultron/rlm/prompt.ts`) has a 5,500-character budget enforced by a test.

## Code

- No `any` unless unavoidable. Top-level imports only (no `await import()` or `import("pkg").Type`).
- Only erasable TypeScript in `packages/*/src`, `packages/*/test` and `packages/coding-agent/examples`: no parameter
  properties, `enum`, `namespace`, `import =` or `export =`.
- In `packages/coding-agent`, resolve package assets through `src/config.ts` helpers, not `__dirname`.
- Keybindings go in `DEFAULT_EDITOR_KEYBINDINGS` / `DEFAULT_APP_KEYBINDINGS`, never hardcoded key checks.
- `packages/ai/src/models.generated.ts` is generated: change `packages/ai/scripts/generate-models.ts` and regenerate.
- Check `node_modules` for external API types instead of guessing. Fix outdated-dep type errors by upgrading, not by
  removing code.
- Ask before removing functionality that looks intentional. Backward compatibility only when asked.

## Dependencies

- Dependency and lockfile changes are reviewed code. Pin direct external deps to exact versions.
- `npm install --ignore-scripts` (or `npm ci --ignore-scripts`); no lifecycle scripts unless asked. Refresh the
  lockfile with `npm install --package-lock-only --ignore-scripts`.
- Regenerate `packages/coding-agent/npm-shrinkwrap.json` with `node scripts/generate-coding-agent-shrinkwrap.mjs`;
  deps with lifecycle scripts need an explicit allowlist entry there.
- Lockfile commits need `PI_ALLOW_LOCKFILE_CHANGE=1`; only when the lockfile change is meant to be committed.
- When updating `undici`, read its release notes for the target version first.

## Git

Several agent sessions may work in this checkout at once.

- Commit only when the user asks, and only files you changed: stage explicit paths, check `git status` first.
- No `Co-Authored-By` or "Generated with" lines in commits or PRs.
- Never: `git reset --hard`, `git checkout .`, `git clean -fd`, `git stash`, `git add -A`, `git add .`,
  `git commit --no-verify`, force pushes. On rebase conflicts in files you didn't change, stop and ask.
- Parallel work goes in a git worktree (`git worktree add -b <branch> ../<dir> ultron`). Give it a real
  `node_modules` directory with per-entry symlinks to the main checkout's, pointing the workspace scopes
  (`@ultron`, `@bryandlp`, `pi-extension-*`) at the worktree's own packages; a single `node_modules` symlink makes a
  worktree build bundle the main checkout's code.
- To inspect a PR, use `gh pr view`/`gh pr diff` and `git show`; don't switch branches unless asked.
- Post issue or PR comments with `--body-file`, in the user's tone.

## Changelog

`packages/*/CHANGELOG.md`, entries under `## [Unreleased]` (`### Breaking Changes`, `Added`, `Changed`, `Fixed`,
`Removed`); append to existing subsections, never edit released sections. Link issues and PRs in
`alfredosdpiii/ultron`. Only on `ultron`/`main`, not on feature branches.

## Pointers

- Interactive TUI testing with tmux: [.pi/skills/interactive-testing.md](.pi/skills/interactive-testing.md).
- Releases: [.pi/skills/release.md](.pi/skills/release.md).
- Design and status: `docs/implementation-status.md`, `docs/ultron-architecture.md`.

If the user's instructions conflict with this file, follow the user after confirming.
