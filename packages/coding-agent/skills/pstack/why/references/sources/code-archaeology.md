# Code archaeology (git, gh, in-repo)

## What this source contains

- Commit history: messages, dates, authors, diffs
- PR descriptions, review comments and discussion (through `gh`)
- Inline comments, TODOs, FIXMEs, deprecation notes
- ADRs, if the repo keeps them
- Tests: names and assertions often encode the edge case that motivated a change
- Files changed in the same commits (co-change signal)
- CHANGELOG entries and release notes
- Ticket IDs in commit messages and PR bodies

The most trustworthy source, tied directly to the code, and the most complete. Always available.

## How to search it

All through `bash` in the REPL; run independent commands together with `asyncio.gather`:

```python
target, symbol, literal = "src/core/retry.ts", "retryWithBackoff", "MAX_ATTEMPTS = 5"
history, pickaxe, blame = await asyncio.gather(
    bash(f'''git log --follow --oneline -- {target}'''),
    bash(f'''git log -S '{literal}' --format='%h %ad %an %s' --date=short -- {target}'''),
    bash(f'''git blame -L 40,80 --date=short {target}'''),
)
print(pickaxe)
```

- `git log -G '<regex>'` for patterns; `git show <hash>` for one commit's diff; `git log <old>..<new> -p -- <file>` for a range.
- `git log -1 --format=%B <hash>` for the full message and its PR number.
- `gh pr view <n> --json title,body,author,createdAt,mergedAt,labels,closingIssuesReferences,comments,reviews,files`. The `reviews` and `comments` fields hold most of the signal.
- Out-of-band docs: `rg -l -i 'architecture.decision' --glob '*.md'`; TODOs near the target: `rg -n -C2 '(TODO|FIXME|HACK|XXX|NOTE)' <file>`; tests: `rg -l '<symbol>' --glob '*test*'`.

Print one line per commit or PR and read only the ones that bear on the question. A long PR thread goes to `rlm.load(text=...)` and `h.search` before you read it.

## What good evidence looks like

- A PR description that explains the problem, not just the change
- A review thread where alternatives were debated
- A comment next to the target line explaining a non-obvious constraint
- A test named for the edge case that motivated the code
- A commit message citing a ticket or incident ID
- A CHANGELOG entry with the user-visible rationale

## Common pitfalls

- **Squash merges** lose branch commits. Fall back to the PR body and comments.
- **Misleading messages.** "Small refactor" can hide a behavior change. Read the diff.
- **Cargo-culted patterns.** If the pattern exists earlier in the codebase, find where it started and investigate that commit.
- **Bot commits** (Dependabot, Renovate, backports) rarely carry motivation.
- **Code as evidence of intent.** "The function is named X" is not evidence of why it exists.

## What to record

Every commit, PR or comment that bears on the question: the exact text, hash / PR number / file:line, author and date, and whether it is direct or circumstantial.
