# Automated pull-request reviews (`ultron autoreview`)

`ultron autoreview` reviews a pull request as soon as one of your logged-in `gh` accounts is requested as reviewer
or @mentioned on it, and posts the review under that account. It is built for speed (a typical pull request is
reviewed within two minutes of being picked up) and for few false alarms: every finding is checked against the real
source by a second model call before it is posted.

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
| `ultron autoreview review --repo-dir <dir> --base <sha> --head <sha>` | Review the diff between two commits of a local repository, with no GitHub access. `--json` prints one JSON object on stdout (logs go to stderr); `--model`, `--verify-model`, `--budget` override the settings. |
| `ultron autoreview status` | Accounts, last poll, queue, and recent reviews with their timings and cost. `--json` for the raw state. |
| `ultron autoreview install` / `uninstall` | Write or remove a user service that runs `ultron autoreview run` (see below). |

## What triggers a review

Per account, every `autoreview.pollSeconds` (45 by default):

- the notifications API, for pull requests with the reason `review_requested`, `mention` or `team_mention`
  (conditional requests, and GitHub's `X-Poll-Interval` is honoured);
- every fifth cycle, three searches so nothing is missed: open pull requests requesting the account's review, pull
  requests mentioning it, and open pull requests it reviewed that changed since.

A pull request found this way is reviewed when:

- the account has not reviewed it yet and a review is requested or the account is mentioned (the mention is looked
  up in the description, comments, review comments and review bodies; a notification without one is ignored);
- the head commit differs from the commit of the account's last review;
- the review was requested again after the last review;
- a new mention arrived after the last review.

It is skipped when the same head commit was already reviewed and nothing new asks for it, and when the pull request
is closed or merged, unless it was freshly mentioned (it then gets a comment-only review).

A commit is tried three times. After the third failure one comment says it could not be reviewed, and that commit is
left alone until a new commit or a new mention.

## What it posts

1. **An acknowledgement comment**, once per pull request and head commit, before the review starts, so the author
   knows it was picked up:

   ```markdown
   > *I've read your diff. I have notes.*
   > — Ultron

   Reviewing `1a2b3c4`.
   ```

   The line is picked at random from `autoreview.ackLines`, never the one used last on the same pull request. With
   `autoreview.ackArt`, that text follows in a fenced code block. `autoreview.ack: false` turns the comment off.

2. **One review**, posted in a single request for the reviewed commit:

   - **Approve** when coverage was complete and there is no confirmed blocker or major finding.
   - **Request changes** when there is one (or one from an earlier review is still present).
   - **Comment** when coverage was incomplete (the token budget ran out, frames failed, the repository could not be
     cloned), when the account opened the pull request itself, or when the pull request is closed or merged. It
     never approves on partial coverage.

   Confirmed findings are inline comments on the diff: every blocker, up to five major, five minor and three nits.
   A finding whose line is not in the diff moves to the nearest diff line within three lines of a hunk, or else
   into the summary. A GitHub suggestion block is used only when the fix is an exact replacement for the commented
   lines and those lines are all in the diff. Findings the verifier could not decide are listed in the summary
   only (at most five) and never count toward the verdict. A finding somebody else already raised (same file,
   nearby line, similar claim) is not posted again; the summary lists it as "also raised by @name".

   The summary has the verdict, counts by severity, findings not posted inline, what was not checked, one line of
   timing and cost, and ends with `Automated review by Ultron` (`autoreview.signature`).

After posting, the review is read back and any inline comment GitHub dropped is posted again on its own. If the head
commit moved while the review ran, the review is discarded and the pull request is queued again.

### Re-reviews

When the head moved since the account's last review, only the changes since the reviewed commit are reviewed (after
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
| `autoreview.model` | `review.model`, then `rlm.frameModel`, then the default model | `provider/model` of the finder frames. |
| `autoreview.verifyModel` | the finder model | `provider/model` of the verifier frames. |
| `autoreview.budget` | `300000` | Token cap of one review. |
| `autoreview.dryRun` | `false` | Write would-be reviews to `autoreview/dry-run/` instead of posting. |
| `autoreview.ack` | `true` | Post the acknowledgement comment. |
| `autoreview.ackLines` | 17 built-in lines | The lines one is picked from. |
| `autoreview.ackArt` | empty | Text (an ASCII-art banner) appended to the acknowledgement in a fenced code block. |
| `autoreview.signature` | `true` | End the summary with `Automated review by Ultron`. |

```json
{
  "autoreview": {
    "accounts": ["my-bot"],
    "model": "anthropic/claude-sonnet-4-5",
    "budget": 200000
  }
}
```

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
| `~/.ultron/agent/autoreview/logs/` | One log file per day. |
| `~/.ultron/agent/autoreview/dry-run/` | Would-be reviews (`--dry-run`). |
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
  material they read. Nothing from the reviewed repository is executed, and its settings, extensions and context
  files are not loaded.
- GitHub's rate limits (`Retry-After`, the primary limit's reset time) pause the account that hit them.
