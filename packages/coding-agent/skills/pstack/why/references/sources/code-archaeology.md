# Code archaeology (git, gh, in-repo)

## What this source contains

- Commit history: messages, dates, authors, diffs
- PR descriptions, review comments and discussion (through `gh`)
- Inline comments, TODOs, FIXMEs, deprecation notes
- ADRs, if the repo keeps them
- Tests, whose names and assertions often encode the edge case that motivated a change
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
- Out-of-band docs: `rg -l -i 'architecture.decision' --glob '*.md'`; TODOs near the target: `rg -n -C2 '(TODO|FIXME|HACK|XXX|NOTE)' <file>`; tests: `rg -l '<symbol>' --glob '*test*'`.

Print one line per commit or PR and read only the ones that bear on the question. A long PR thread goes to `rlm.load(text=...)` and `h.search` before you read it.
