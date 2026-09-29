# `/review`: code review with verified findings

`/review` is an optional command. It runs only when you type it; nothing about it is in the system prompt of
ordinary turns. It reviews a change in two phases: specialist reviewers look for problems, then a verifier reads the
code each finding cites and confirms or rejects it. Only confirmed findings are reported as findings.

```text
/review                         # uncommitted changes + this branch since its merge base with the default branch
/review main                    # changes since the merge base with main (or any branch, tag or commit)
/review 123                     # pull request 123, via `gh pr diff` (also #123 or the PR URL)
/review src/parser.py docs/     # only these paths (combine with a ref or PR; `--` ends options)
/review --only sec,bugs         # choose reviewers
/review --budget 150k           # token cap for all frames (default 300k)
/review --model provider/model  # model for the frames (default: the session's model)
/review --deep                  # re-check up to 3 uncertain findings with a sub-agent (--deep=N for N)
/review --plan                  # show scope, chunks and frame plan; no model calls
/review 123 --post              # prepare posting to the PR; it is posted only after you say yes
```

It works the same in the TUI, `ultron -p "/review"`, and RPC prompts. Defaults can also come from
`ULTRON_REVIEW_BUDGET`, `ULTRON_REVIEW_MODEL` and `ULTRON_REVIEW_ONLY`.

## What happens

1. **Scope.** The diff comes from git: by default the working tree (staged, unstaged and untracked files) plus
   the commits on the current branch since its merge base with the default branch (`origin/HEAD`, else `main`,
   `master`, `trunk` or `develop`). On the default branch itself it is just the uncommitted changes. A PR number
   uses `gh pr diff`; code around the diff is read from the working tree when it is at the PR head, else from the
   head commit if it is in the clone. With no changes, `/review` says so and makes no model call.
2. **Chunks.** Lockfiles, generated, vendored, binary and deleted files are skipped and listed. Each remaining file's
   hunks are rendered with new-file line numbers in a gutter plus up to 12 unchanged lines of the real file around
   each hunk, and packed into chunks of about 14,000 characters (a large hunk is split).
3. **Find.** One `rlm.map` over (reviewer x chunk). Each frame gets one reviewer's instructions and checklist and
   one chunk, and must return a JSON array of `{file, line, severity: blocker|major|minor|nit, category, claim, why,
   suggested_fix, confidence}` (at most 8; malformed replies are re-asked). Replies are bounded in code: the file is
   pinned to the chunk's, out-of-range lines are clamped, and long strings are cut.
4. **Dedupe.** Findings on the same file within 3 lines that share a category, or make the same claim, merge into
   one that keeps the most severe wording and lists every reviewer that raised it.
5. **Verify.** One verifier frame per finding, most severe first, sees the finding, the source 15 lines around the
   cited line, the diff hunk, and where the names involved (the enclosing function, what the line calls) are
   defined or used elsewhere, found with `git grep`. It returns `{verdict: confirmed|rejected|uncertain, evidence,
   corrected_line?}`. Rejected findings are dropped. A confirmation whose evidence does not quote the source is
   downgraded to uncertain, and so is a frame that failed or ran out of budget.
6. **Report.** Confirmed findings grouped by severity, each with `file:line`, the claim, the source line quoted by
   code (not by the model), the verifier's evidence, why and the fix; uncertain findings listed apart; counts
   (raised, after dedupe, confirmed, rejected, uncertain), cost (frames and tokens against the cap), the saved
   report's path (`.git/ultron-review/`), and what the review did not check.

## Reviewers

| Key | Reviewer | Looks for |
|---|---|---|
| `bugs` | Correctness | bounds and conditions, missing values, swallowed errors, async misuse, broken contracts, leaks |
| `security` | Security | injection, missing access checks, secrets, unsafe parsing, SSRF, permissive defaults |
| `arch` | Architecture and maintainability | public surface changes, layering, duplication, hardcoding, error model, complexity |
| `tests` | Tests and QA | untested behavior changes, assertions that cannot fail, weakened or flaky tests, missing edges |
| `ai` | AI and LLM integration | prompt injection, model output run as code, unbounded output and fan-out, missing timeouts and budgets, unvalidated structured output |

Aliases: `sec`, `bug`/`correctness`, `architecture`/`maint`, `qa`/`test`, `llm`. The AI reviewer only runs on chunks
that mention model or LLM APIs, and documentation files go only to the security reviewer (for secrets); the report
says how many chunks each skipped.

## Frames, not sub-agents

Every reviewer and verifier is an `rlm.map` frame: one private sub-model request with no tools and no transcript.
A finder needs only its chunk, and a verifier needs only the cited lines, the hunk and the callers, all of which
code can collect first. A sub-agent would re-send a system prompt and its own transcript on every turn to learn the
same things. The one place exploring pays is a finding the verifier could not decide from its views, so `--deep`
sends those (the most severe first, 3 by default) to `rlm.spawn` sub-agents that may read the repository and run
quick read-only commands. Their verdicts go through the same filter.

## Budget and limits

All frames share one token cap (`--budget`, default 300,000): the find phase may use 60% of it and verification
gets what the find phase left. Frames are planned against estimates before any request (code before tests before
docs; most severe findings verified first), and whatever does not fit is reported as not checked, never guessed.
Each frame has a 4-minute timeout. Frames also count toward the session's turn, token and cost limits; a frame
stopped by them comes back `Incomplete` and is reported as not checked.

## Posting

`--post` never posts by itself. With a PR, the report ends with "Post pending", and the model asks whether to post;
only after you answer yes does it call `review_api.post(review, confirm=True)`, which runs
`gh pr comment <n> --body-file <report>`. In `-p` mode there is no later answer, so nothing is posted.

## Where it lives

- `packages/coding-agent/src/ultron/rlm/review_api.py`: scoping, parsing, chunking, planning, dedupe, the verify
  filter, the report and posting (plain functions, unit-tested).
- `packages/coding-agent/src/ultron/rlm/review_prompts.py`: the reviewer checklists and the finder, verifier and
  deep-check instructions.
- `packages/coding-agent/src/ultron/review.ts`: expands `/review ...` into a one-cell prompt; applied in the worker's
  AgentController, so every client gets it. The TUI lists it with the other slash commands, and RPC `get_commands`
  reports it.
- Tests: `packages/coding-agent/test/ultron-review.test.ts`, including an end-to-end review in a real kernel with a
  scripted provider (one real bug confirmed, one planted false finding rejected).
