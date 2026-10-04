# Automated pull-request reviews (`ultron autoreview`)

`ultron autoreview` reviews a pull request as soon as one of your logged-in `gh` accounts is requested as reviewer
or @mentioned on it, and posts the review under that account. It is built for speed (a typical pull request is
reviewed within two minutes of being picked up) and for few false alarms: every finding is checked against the real
source by a second model call before it is posted.

By default it reviews in two passes and posts one review: a fast pass over the changed lines, then a deep pass that
follows the change into the rest of the repository (callers, called helpers, tests, sibling code, documents) through
read-only lookups. Nothing from the reviewed repository is executed.

It is the same finder-then-verifier pipeline as `/review` (`docs/review.md` in the Ultron repository), run
directly: no root model turn, only the pipeline's own frames.

## Quick start

```bash
gh auth login                      # once per account; every logged-in account is used
ultron autoreview once --dry-run   # one poll; write the would-be reviews instead of posting
ultron autoreview run              # poll and review until stopped
```

Posting is on by default. `--dry-run` (or the setting `autoreview.dryRun`) writes each would-be review to
`~/.ultron/agent/autoreview/dry-run/` as JSON and markdown and posts nothing.

## Commands

| Command | What it does |
|---|---|
| `ultron autoreview run` | The loop: poll every account, review what is due, until stopped. One per agent directory. |
| `ultron autoreview once` | One poll cycle, review what it finds, then exit. `--json` prints the outcomes. |
| `ultron autoreview review <owner/repo#N \| URL>` | Review one pull request now, whether or not it was requested. `--account <login>` picks the account (default: the host's active one); `--dry-run`, `--json`. |
| `ultron autoreview review --repo-dir <dir> --base <sha> --head <sha>` | Review the diff between two commits of a local repository, with no GitHub access. `--json` prints one JSON object on stdout (logs go to stderr); `--mode`, `--model`, `--verify-model`, `--deep-model`, `--budget`, `--thinking`, `--verify-thinking`, `--deep-thinking` and `--deadline` override the settings. |
| `ultron autoreview doctor` | The sandbox that test execution would use, with a self-check of its isolation. |
| `ultron autoreview status` | Accounts, last poll, queue, and recent reviews with their timings and cost. `--json` for the raw state. |
| `ultron autoreview install` / `uninstall` | Write or remove a user service that runs `ultron autoreview run` (see below). |

## What triggers a review

Per account, every `autoreview.pollSeconds` (45 by default):

- the notifications API, for pull requests with the reason `review_requested`, `mention` or `team_mention`
  (conditional requests, and GitHub's `X-Poll-Interval` is honoured);
- every fifth cycle, two searches so nothing is missed (open pull requests requesting the account's review, and
  pull requests mentioning it), plus a look at the open pull requests on which the account's last review requested
  changes.

A pull request found this way is reviewed when:

- the account has not reviewed it yet and its review is requested or the account is @mentioned. The mention is
  looked up in the description, comments, review comments and review bodies; a notification without one is ignored.
  A team request counts only when the account is a member of the requested team (read through the API; when the
  membership cannot be read, the request is skipped and logged);
- the review was requested again after the last review;
- a new mention arrived after the last review;
- new commits were pushed since the account's last review and the account is still (or again) a requested reviewer,
  was mentioned again, or its last review requested changes (it is then blocking the pull request, so the block is
  lifted or confirmed).

It is skipped when:

- the same head commit was already reviewed and nothing new asks for it;
- new commits arrived after an approval or a comment-only review and nothing asks for another look: one review ends
  the account's part;
- it is a draft, unless the account is explicitly @mentioned;
- it is closed or merged, unless it was freshly mentioned (it then gets a comment-only review and no
  acknowledgement).

A commit is tried three times. After the third failure one comment says it could not be reviewed, and that commit is
left alone until a new commit or a new mention.

## What it posts

1. **An acknowledgement comment**, once per pull request and head commit, posted the moment the pull request is
   found to be due: acknowledgements do not wait for a free review slot, so the author hears back within seconds
   even when several reviews are queued:

   ```markdown
   > *I've read your diff. I have notes.*
   > — Ultron

   Reviewing `1a2b3c4`.
   ```

   The line is picked at random from `autoreview.ackLines`, never the one used last on the same pull request.
   Ultron's logo follows in a fenced code block (`autoreview.ackArt`: `"none"` leaves it out, any other text
   replaces it). `autoreview.ack: false` turns the comment off. Closed and merged pull requests get none.
   `ultron autoreview status` shows, per review, the time from the notification to the acknowledgement and from
   the acknowledgement to the posted review.

2. **One review**, posted in a single request for the reviewed commit:

   - **Request changes** when a confirmed finding is at `medium` or above (`autoreview.blockAt`), or one from an
     earlier review is still present at that level.
   - **Approve** when coverage was complete and there is none.
   - **Comment** when coverage was incomplete (frames failed, a token cap or deadline you set ran out, the
     repository could not be cloned), when the account opened the pull request itself, or when the pull request is
     closed or merged. It never approves on partial coverage.

   **Few, heavy comments.** Confirmed findings are ranked by level and by the strength of their evidence (a test
   the host ran, then source quoted from outside the diff, then the diff alone), and at most
   `autoreview.maxComments` (5) are posted inline. A finding below `high` whose evidence is only the diff gets a
   slot only when one is left; nits are never inline. The same problem in several places is one comment that lists
   the other places. What is not posted is counted in one closing line.

   An inline comment reads `[level] what is wrong, with the evidence. The fix.`, in plain sentences, about 600
   characters at most. It sits on a diff line: its own, or the nearest within three lines of a hunk. A GitHub
   suggestion block is used only when the fix is an exact replacement for the commented lines and those lines are
   all in the diff. Findings the verifier could not decide are never posted and never count toward the verdict. A
   finding somebody else already raised (same file, nearby line, similar claim) is not repeated.

   The body has no headings or tables: a paragraph on what was checked and holds; a paragraph with the verdict and
   what must be resolved before merge, each item with its file and line; a line on the non-blocking notes; what was
   not checked; one line of timing and cost; and `Automated review by Ultron` (`autoreview.signature`).

After posting, the review is read back and any inline comment GitHub dropped is posted again on its own. If the head
commit moved while the review ran, the review is discarded and the pull request is queued again.

### Re-reviews

When a review is due after the head moved since the account's last review, only the changes since the reviewed commit are reviewed (after
a force-push, the whole pull request again). Each finding posted earlier is re-checked against the new source:
fixed, still present, or no longer applicable. The summary has a table of them, and the account's own review
threads whose finding is fixed are resolved (by the thread id stored when the comment was posted).

## Settings

Global settings only (`~/.ultron/agent/settings.json`), so a repository under review cannot change how it is
reviewed.

| Setting | Default | Description |
|---|---|---|
| `autoreview.accounts` | every logged-in account | Logins (or `host/login`) to review as. |
| `autoreview.pollSeconds` | `45` | Seconds between polls (minimum 20). |
| `autoreview.concurrency` | `3` | Pull requests reviewed at once (maximum 8). |
| `autoreview.model` | unset: `review.model`, then `rlm.frameModel`, then the default model | `provider/model` of the finder frames. |
| `autoreview.verifyModel` | the finder model | `provider/model` of the verifier frames. |
| `autoreview.mode` | `"both"` | `fast`: review the diff only. `deep`: only the investigation beyond the diff. `both`: the fast pass, then the deep one with its findings as leads; one review is posted. |
| `autoreview.deepModel` | the finder model | `provider/model` of the deep pass's investigator frames. |
| `autoreview.deepThinking` | `"high"` | Their thinking level. |
| `autoreview.blockAt` | `"medium"` | A confirmed finding at this level or above makes the review request changes (`critical`, `high`, `medium`, `low`, `nit`). |
| `autoreview.maxComments` | `5` | Inline comments per review at most; the rest are counted in the body. |
| `autoreview.deepRounds` | `4` | Lookup rounds one investigator may take (maximum 8). |
| `autoreview.budget` | none | Optional token cap of one review. Unset, no pass is refused for tokens. |
| `autoreview.thinking` | `"low"` | Thinking level of the finder frames (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`). |
| `autoreview.verifyThinking` | `"low"` | Thinking level of the verifier frames. |
| `autoreview.frameConcurrency` | `8` | Model requests of one review in flight at once, finder and verifier frames alike (maximum 16). Lower it if your provider rate-limits. |
| `autoreview.deadlineSeconds` | `0` (none) | Optional limit on how long one review may take (minimum 30). Unset, the review waits for every finder and verifier frame. See below. |
| `autoreview.frameTimeoutSeconds` | `0` (none) | Optional limit on how long one frame may take before it is retried or given up (minimum 10). |
| `autoreview.dryRun` | `false` | Write would-be reviews to `autoreview/dry-run/` instead of posting. |
| `autoreview.ack` | `true` | Post the acknowledgement comment. |
| `autoreview.ackLines` | 17 built-in lines | The lines one is picked from. |
| `autoreview.ackArt` | `"logo"` | Art appended to the acknowledgement in a fenced code block: `"logo"` (Ultron's half-size logo), `"none"` or `false` (no art), or any other text, used verbatim. |
| `autoreview.signature` | `true` | End the summary with `Automated review by Ultron`. |

```json
{
  "autoreview": {
    "accounts": ["my-bot"],
    "model": "claude-code/haiku",
    "verifyModel": "cliproxyapi/glm-5.3-flash"
  }
}
```

The two model ids are examples: use `provider/model` names your own providers offer (`ultron --list-models`).

## The deep pass: beyond the diff, read-only

Many defects are invisible from the changed lines alone: a new `except` that can never fire because the helper it
wraps returns a sentinel instead of raising; new members that an existing parametrize list in another test file
does not include; a comment that claims a validation the code never performs; a migration written for one database
branch only. The deep pass looks for these.

1. **Map.** Without a model, the host collects the claims the change makes (its title and description, its commit
   messages, the comments, docstrings and documents it changes) and extracts from the diff the names it defines,
   changes and calls, its constants, environment variables, flags and table names. It looks them up in the
   repository at the reviewed commit: who uses each name, what the called helpers do, which tests mention them and
   how those test files are parametrized, sibling files, and the documents and configs that name them. The result
   is a bounded brief with file:line anchors, led by the claims and where each has to hold.
2. **Investigators.** The method is claim-driven. One frame per part gets the diff, the brief and the fast pass's
   findings as leads; parts that do not apply to the change are skipped:
   - `claims`: for each claim, find where it must be true (the path that failed, every other reader and writer)
     and check it there; what holds feeds the review's first paragraph.
   - `siblings`: other producers and consumers, parallel implementations, families (keywords, enum members,
     regexes), and conventions the same file or module already follows that the change breaks or does not extend.
   - `deployment`: environment variables, flags, manifests, CI workflows, infrastructure and dependency pins the
     change relies on: set and enforced where the feature runs?
   - `tests`: would a test fail if the new behaviour were removed? (settled with a mutation check when tests can
     run); presence-versus-value assertions; what sibling tests set up.
   - `inputs`: a concrete input or state that defeats a new guard, regular expression, limit or parser, with the
     path it takes.

   Each answers with findings and with requests: `read` (lines of a file), `grep`, `list` (a directory),
   `definition` and `references` (of a name), and three history lookups: `history` (the commits that touched a
   file), `blame_range` (those that last changed a line range) and `pickaxe` (those that added or removed a
   string). The host checks each request (a tracked file of the reviewed commit, no path outside the repository,
   bounded lines, hits and bytes per round), answers it, and asks the frame again, for at most
   `autoreview.deepRounds` rounds. An investigator that stops in its first round having looked at fewer than three
   things is sent back once.
3. **Evidence.** Every deep finding cites file and line with the quoted source line. The host checks that each quote
   is at its cited line; a finding with a wrong quote, or with none, is dropped. A finding whose evidence is all
   inside the diff counts like a fast one.
4. **Verification** is the same as for the fast pass (the verifier also sees the cited source), with the same
   level rules. A deep finding replaces the fast finding it extends.

The review then opens with what was traced and found to hold. Findings on lines outside the diff cannot be inline
comments; they are named in the body with their file and line.

The lookups are read-only by construction: the frames have no tools, and the host runs only `git grep`, `git show`,
`git ls-tree` and `git log` on the checkout. (Running the project's tests is a separate, sandboxed step: see below.) If there is no checkout, or the deep pass fails, the fast review is posted as before.

With `--json`, findings carry `source` (`fast` or `deep:<part>`), `evidence` and `howVerified`; the object has
`assurance`, `mode`, and `timing.investigators` (rounds, lookups, time and tokens per investigator).

## Running the project's tests

The deep pass may run the reviewed project's tests. Models still have no shell: running tests is something the host
does, in a closed form, inside a sandbox.

- **Automatic run.** After the map, the host detects the test runner (pytest; vitest, jest or the `test` script of
  `package.json`; `go test`; `cargo test`; a Makefile `test` target) per test file, by the file's type and its
  nearest manifest (a TypeScript test belongs to the closest `package.json` that names a runner, so each package of
  a monorepo runs its own tests in its own directory), and runs the test files the map tied to the change, plus test files the change touches, at the reviewed commit. The results go into the investigators' brief.
  When a test fails, the same selection is run at the base commit: a test that fails now and passed before is a
  major finding by itself; one that already failed is not this change's.
- **On request.** An investigator may ask for `run_tests` (existing test files, an optional test-name selector)
  and `mutation_check` (the host replaces one source line, runs the named tests, reports whether any failed, and
  restores the line: a mutant nobody catches shows that nothing pins that line). A finding may cite a run as its
  evidence.
- **Limits.** At most `autoreview.testRuns` executions per review (default 6), each stopped after
  `autoreview.testTimeoutSeconds` (default 300), output trimmed.

**The sandbox is mandatory.** Every execution runs in a temporary export of the commit (never your checkout),
with no network and no credentials: the environment is built from scratch (no token, key, cloud or SSH variable),
HOME is an empty temporary directory, and your real home, the agent directory, gh's configuration, other
repositories and the Docker socket are not visible. The mechanism is the strongest available: bubblewrap (an empty
root, system directories read-only, the export as the only writable directory, every namespace unshared), else
rootless `unshare` namespaces, else Docker with a local image you name in `autoreview.testImage` (`--network none`,
all capabilities dropped). With none of them, tests are not run and the review says "tests not run: no sandbox
available". `ultron autoreview doctor` shows the mechanism and runs a self-check inside it (network unreachable, a
canary file in your home unreadable, a token-like variable absent).

**No network means no installing.** If the project's dependencies are not on the machine, the tests cannot run;
the review then says "tests could not run: missing dependencies" and reports no failure. To give a repository its
dependencies, point `autoreview.testEnv` at a pre-built environment (a virtualenv, a `node_modules` directory); it
is bound read-only. Nothing is ever installed automatically.

**Eligibility.** Tests run only for repositories the reviewing account can push to, or whose owner is listed in
`autoreview.testOwners`; elsewhere the deep pass stays read-only. `autoreview.runTests: false` turns it off
entirely. The local entry (`review --repo-dir`) runs them unless `--no-run-tests` is given; `--test-env <dir>` binds
an environment.

| Setting | Default | Description |
|---|---|---|
| `autoreview.runTests` | `true` | Run tests in the deep pass where eligible and a sandbox exists. |
| `autoreview.testOwners` | none | Owners whose repositories' tests may run without push access. |
| `autoreview.testRuns` | `6` | Test executions per review. |
| `autoreview.testTimeoutSeconds` | `300` | Wall-clock limit of one execution. |
| `autoreview.testEnv` | none | `{"owner/repo": "/path/to/env"}`: pre-built environments, bound read-only. |
| `autoreview.testImage` | none | A local Docker image, used only when neither bubblewrap nor `unshare` works. |

## Levels

Every finding has one of five levels:

| Level | Meaning |
|---|---|
| `critical` | Data loss or exposure, a security hole, an outage or a wrong result on a main path, with the scenario shown. |
| `high` | A wrong result, an exception or a regression for an input or state the author plainly means to support, with the scenario stated: input or state, what happens, what should. |
| `medium` | A real gap, with evidence: a behaviour or a claim that does not hold in some supported case; a convention of the same module broken; a new behaviour with no test that would fail if it were removed; configuration not set where the feature needs it. |
| `low` | Hardening and robustness. |
| `nit` | Style and wording. |

The review asks for changes when a confirmed finding is at or above `autoreview.blockAt` (default `medium`), so the
level is checked twice. The finder states a concrete failing scenario for anything it rates critical or high. The
verifier then reads the source, gives its own level and says whether that scenario really fails. The posted level is
the verifier's, and:

- critical and high stand only when the scenario holds in the source; otherwise the finding is low;
- a finding without a concrete scenario is never above medium; neither are missing tests, nor maintainability and
  architecture findings unless their scenario is a real failure;
- a behaviour change that is the evident point of the pull request is not a defect: at most a low "confirm this is
  intended", unless it breaks a caller shown in the source;
- the verifier may also raise a finding: a wrong result on an input the author means to support is high even if the
  finder said low.

With `--json`, every finding has `level` and `finderLevel` on this scale, plus `scenario`. `severity` and
`finderSeverity` remain, on the older four-name scale: critical is `blocker`, high is `major`, medium and low are
`minor`, nit is `nit`. (Medium has no older name; note that a medium finding asks for changes while its `severity`
reads `minor`.) The older names are still accepted wherever a level is read.

## Speed: slices, retries and optional limits

- Small files are packed into one slice (up to about 14,000 characters, a file never split further), so a small
  pull request costs one finder frame per reviewer, not one per reviewer and file. Each finding still names its file
  and line.
- Every frame is one model request. A rate limit (429), a server or network error is retried twice, with jittered
  backoff or after the time the provider asks for, before the pass is listed as not checked.
- By default there is no token cap, no per-frame timeout and no deadline: the review waits for every finder and
  verifier frame and posts when all are done. Each limit is opt-in:
  - `autoreview.budget` (or `--budget`): the cap counts what was really spent plus a bounded grant for each request
    in flight; a pass is refused only when that leaves no room for it.
  - `autoreview.frameTimeoutSeconds`: a frame that takes longer is retried, then given up.
  - `autoreview.deadlineSeconds` (or `--deadline`): at three quarters of it unfinished finder passes are given up;
    what was found is verified in the time left; the review is posted as incomplete (so never an approval) with
    the unfinished passes listed under "Not checked".
- With `--json`, `timing.frames` lists every frame: `{phase, reviewer, ms, status, retries}`.

## Run it as a service

```bash
ultron autoreview install
```

writes `~/.config/systemd/user/ultron-autoreview.service` (`Restart=always`; on macOS, a launchd agent in
`~/Library/LaunchAgents/`) and prints the command that enables it. It does not enable it for you:

```bash
systemctl --user daemon-reload
systemctl --user enable --now ultron-autoreview.service
```

`ultron autoreview uninstall` removes the file and prints how to stop the service.

## How to stop it

- Foreground: Ctrl+C. Running reviews finish and are posted first.
- Service: `systemctl --user disable --now ultron-autoreview.service` (macOS: `launchctl unload -w <plist>`).
- Keep it running without posting: set `autoreview.dryRun` to `true`.
- Stop reviewing as one account: list the others in `autoreview.accounts`, or `gh auth logout` that account.

## Files

| Path | Contents |
|---|---|
| `~/.ultron/agent/autoreview/state.json` | Per account and pull request: last reviewed and acknowledged commits, attempts, posted findings with their comment and thread ids; recent reviews. |
| `~/.ultron/agent/autoreview/logs/` | One log file per day (removed after 14 days). |
| `~/.ultron/agent/autoreview/dry-run/` | Would-be reviews (`--dry-run`; removed after 14 days). |
| `~/.ultron/agent/autoreview/sessions/` | Frame traces of the engine (kept seven days). |
| `~/.cache/ultron-autoreview/<host>/<owner>/<repo>.git` | Cached blob-less clones; a worktree per review under `worktrees/`, removed afterwards. |

## Privacy and safety

- **Pull request content is sent to the configured model provider**: the diff, the code around it, the title and
  description, CI status, guideline files (`AGENTS.md`, `CLAUDE.md`, `CONTRIBUTING.md`) and other people's review
  comments. Do not point it at repositories whose code may not leave your machine, or choose a provider you trust
  with them.
- Tokens come from `gh auth token` and reach child `gh` and `git` processes only through their environment. They
  are never written to disk, state, logs, git config or a command line, and are masked in log output.
- Everything read from a pull request (description, comments, the mention itself, repository files) is treated as
  data. The review frames have no tools: they cannot run code, fetch anything or follow instructions in the
  material they read. The repository's settings, extensions and context files are not loaded.
- Code of the reviewed repository is executed in one case only: its tests, by the host, inside the sandbox
  described above, for repositories you can push to (or owners you listed). Without a sandbox nothing is executed.
- GitHub's rate limits (`Retry-After`, the primary limit's reset time) pause the account that hit them.
