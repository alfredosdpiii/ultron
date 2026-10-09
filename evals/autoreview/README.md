# Autoreview benchmark

Measures an automated code reviewer's accuracy and speed on diffs whose bug is known, for `ultron autoreview`.
Any reviewer that can print the same JSON can be scored on the same cases.

The harness and the dataset are here; results are recorded under `acceptance/quality/`
(`<date>-autoreview-bench-<cases>x2-<arms>.json` and a `.md` rendering).

## Method

SWE-bench Verified gives a real repository, a commit with a real bug, the issue that reported it and the gold
patch that fixed it. Each task becomes one review case, of one of two kinds:

| kind | reviewed diff | base | head | a good reviewer |
| --- | --- | --- | --- | --- |
| `buggy` | the reverse of the gold patch: the change that introduces the bug | the upstream tree with the gold patch applied | the upstream tree at the task's base commit (the buggy state) | flags the changed region and requests changes |
| `clean` | the gold patch | the upstream tree at the task's base commit | the tree with the gold patch applied | raises nothing at blocker or major severity and approves |

**Sample.** Tasks whose gold patch changes at most 150 lines (added plus removed) in at most 4 files, with no
binary change and no rename: 494 of the 500. `caseOrder` puts them in one seeded order that goes round the
repositories (the repositories in a seeded order, each repository's tasks in a seeded order, round 1 takes one
task from each repository, round 2 a second from each that still has one, and so on), so the small repositories
are represented as well as Django. The sample is the first `2 x --cases` tasks of that order, alternately a buggy
and a clean case: no task is used twice, and a smaller sample is a prefix of a larger one. Seed
`ultron-autoreview-1`; the default is 20 buggy and 20 clean over all 12 repositories. `--plan` prints the sample.

**Case repositories.** A case is a local git repository with two commits, base and head, checked out at head; the
reviewer gets `--repo-dir --base --head` and nothing else (no issue text, no hint which kind the case is). The
upstream tree comes from a bare mirror per task, `git fetch --depth=30 https://github.com/<repo>.git <base commit>`,
no credentials; a case repository borrows the mirror's objects and holds two commits of its own with fixed author,
date and messages (`Base`, `Update <file> and N more files`), so the same inputs give the same commit ids and the
upstream history, which holds the fix, is not on its branch. A built case is reused; `--build-only` builds the
sample and stops, and after that everything runs offline.

**Extra hunks.** A bare reversion is easier to review than a real pull request, so a buggy case's diff also carries
up to two hunks of unrelated, harmless change (`--noise 0` turns this off). They are real upstream changes: the
non-merge commits in the fetched history before the base commit, newest first; in each, the modified text files in
path order; a file's change is used when the gold patch does not touch the file, it changes at most 30 lines, it
fits in the hunks still wanted, and it reverses cleanly against the case's base. Those changes are taken out of
the case's base, so they appear in the reviewed diff, and the head stays exactly the upstream tree, so ground-truth
line numbers do not move. Clean cases get none: a real defect in an extra hunk would be counted as a false alarm.
In the default sample all 20 buggy cases carry one or two such file changes; the result file lists them
per case (`sample.cases[].noise`).

**Ground truth.** Computed from the gold patch alone (`changeRanges`), in the head's line numbers. For a buggy
case the head is the gold patch's old side, so per file of the gold patch:

- `hunks`: each hunk's old-side span, context lines included (`@@ -a,b` gives lines `a..a+b-1`);
- `changed`: the lines the fix removed or replaced, which are the lines the reviewed diff adds; where the fix only
  added lines (the reviewed diff only deletes them, and deleted lines have no number in the head), the two lines
  around the place they were deleted from;
- a file the fix added does not exist in the head: any finding on that file counts as located; a file the fix
  deleted is entirely new in the head: its range is the whole file.

The task's problem statement is the description of the defect. It is used only by the judge.

**Scoring.** One review per (arm, case, trial).

- Buggy, **caught**: a finding with `verification: "confirmed"` and severity blocker or major whose file is a
  ground-truth file and whose `line..endLine` overlaps a ground-truth hunk widened by 5 lines. Reported as recall,
  with two variants: `+minor` also accepts minor findings; `changed lines` requires the finding within 2 lines of a
  changed line instead of anywhere in the hunk.
- Buggy, **judged** (only with `--judge-model`): each confirmed finding of minor severity or worse (at most 8 per
  review, most severe first) is shown to a judge with the problem statement and the gold patch, and the judge
  answers whether the finding describes that defect, wherever the finding points. Judged recall is the share of
  judged reviews with a blocker or major finding the judge accepted. It is reported next to the location-based
  numbers and never replaces them.
- Clean, **false alarm**: any confirmed blocker or major finding. Findings are also counted by severity, and
  whether the verdict was `approve`.
- **Verdict accuracy**: `request_changes` expected for buggy, `approve` for clean (`comment` is wrong for both).
- **Speed and cost**: wall-clock seconds of the reviewer process (p50, p90, max), and what the reviewer reports:
  its timing split (scope, find, verify), tokens, cost and frames.

A review that times out (`--limit-minutes`, default 20) or prints no valid JSON is an error row: counted, not
scored. Rates are over scored reviews.

## The reviewer contract

```
ultron autoreview review --repo-dir <dir> --base <sha> --head <sha> [--model <provider/model>]
                         [--verify-model <provider/model>] [--budget <tokens>] --json --dry-run
```

prints one JSON object on stdout (logs go to stderr):

```json
{"verdict": "approve|request_changes|comment", "complete": true,
 "findings": [{"file": "", "line": 1, "endLine": 1, "severity": "blocker|major|minor|nit", "category": "",
               "claim": "", "why": "", "suggestedFix": "", "verification": "confirmed|uncertain", "confidence": 0.9}],
 "dropped": {"rejected": 0, "duplicates": 0},
 "timing": {"totalMs": 0, "scopeMs": 0, "findMs": 0, "verifyMs": 0},
 "usage": {"inputTokens": 0, "outputTokens": 0, "costUsd": 0, "frames": 0},
 "model": "", "verifyModel": "", "notChecked": []}
```

`file` is relative to the repository (`./`, `a/`, `b/` prefixes and absolute paths under the case directory are
accepted), `line` is in the head's numbering. An unknown severity counts as nit, anything but `confirmed` as
uncertain. Stray lines before the object are tolerated.

**Other reviewers.** `--reviewer-cmd "<command>"` replaces Ultron's reviewer. The command is split into arguments
(quotes group; no shell) and run from the directory the harness was started in. Without placeholders it gets the
contract's flags appended (`--repo-dir ... --base ... --head ... [--model ...] --json --dry-run`); with a `{repo}`
placeholder nothing is appended and `{repo}`, `{base}`, `{head}`, `{model}`, `{verifyModel}` and `{budget}` are
filled in, so a small adapter script that turns another reviewer's output into the JSON above is enough.
`stub-reviewer.mjs` is such a command: it calls no model and decides by a hash, to test the harness offline.

## Running it

```sh
node evals/swebench/run.mjs setup                # once: the virtualenv and the dataset in the local cache
node evals/autoreview/run.mjs --plan             # the 40 cases and the arms; runs nothing
node evals/autoreview/run.mjs --build-only       # fetch and build the case repositories (about 2 minutes, 1.3 GB)

# offline check of harness, scoring and report
node evals/autoreview/run.mjs --reviewer-cmd "node evals/autoreview/stub-reviewer.mjs" --cases 3 --out /tmp/autoreview-stub

# one arm per model, with the built CLI under test; paid model calls
node evals/autoreview/run.mjs --ultron packages/coding-agent/dist/cli.js \
  --models cliproxyapi/gpt-6.1-sol,cliproxyapi/glm-5.3-flash --verify-model cliproxyapi/gpt-6.1-sol \
  --judge-model cliproxyapi/glm-5.3-flash --run-id first
```

Flags: `--cases N` (per kind, default 20), `--seed`, `--models a,b,c` (one arm per model; without it one arm on
the reviewer's default model), `--verify-model`, `--budget <tokens>`, `--trials K` (default 1), `--concurrency`
(default 2), `--reviewer-cmd`, `--ultron <path>` (a `.js`/`.mjs`/`.ts` path runs through Node, anything else is
executed; default `ultron` on `PATH`), `--judge-model <provider/model>`, `--limit-minutes` (default 20),
`--noise N` (extra hunks per buggy case, default 2), `--only <case or task ids>`, `--run-id`, `--out <dir>`
(default `acceptance/quality`), `--baseline <result.json>` (a gate: an arm both have whose caught rate or verdict
accuracy fell, or whose false-alarm rate rose, by more than 10 points exits 3 after the result is written),
`--plan`, `--build-only`. Unknown flags exit 2 before anything starts.

With `--run-id`, rerunning the same command continues the run: a review that already has a scored record is not
run again (errors and timeouts are), and a missing judgement is added without reviewing again.

Before the first review the harness runs `<ultron> autoreview --help` and stops unless the output names the
command: an Ultron without it would take `autoreview review ...` as a prompt and call a model with it.

**The judge** is one call per finding to the same Ultron CLI in print mode, with no tools, extensions, skills or
context files: `ultron -p --model <judge> --no-session --no-tools --no-extensions --no-skills --no-context-files
--system-prompt <system> <prompt>`. The prompt (`judgePrompt`) holds the problem statement, the gold patch and the
finding, and asks for exactly `{"match": true|false, "reason": "..."}`; a reply without that object is recorded as
a judge error (`match: null`) and counts as no match.

**Isolation.** Every reviewer and judge process gets a private `ULTRON_SERVER_DIR` and `ULTRON_CODING_AGENT_DIR`
under `/tmp/u-ar-*` (short, for the socket path limit), deleted when the process ends. For Ultron's reviewer and
the judge, the agent dir gets copies of the user's `models.json` and `auth.json` (nothing else: no settings,
skills, extensions or memory; Hindsight is off) and `HOME` is an empty directory; the copies are deleted with the
temp dir and never reach the evidence, and the kept output is scrubbed of the key values in those two files. A
`--reviewer-cmd` reviewer gets the private dirs but no credentials and keeps its `HOME`: it uses its own login.

## What it leaves behind

```
~/.cache/ultron-autoreview-bench/        (override with ULTRON_AUTOREVIEW_HOME)
  dataset.jsonl                          the dataset with gold patches (export_dataset.py)
  mirrors/<owner>__<name>/<commit>.git   the upstream tree and 30 commits of history, per task
  cases/<kind>-<task>/                   the case repositories
  built/<kind>-<task>.json               base and head commit ids and the extra hunks of a built case
  runs/<run-id>/<arm>/<case>/trial-N/    record.json, stdout.json, stderr.txt, judge-<finding>.txt
  runs/<run-id>/result.json
```

The gold patches are in this directory, not in the case repositories. The SWE-bench harness's own export
(`~/.cache/ultron-swebench/dataset.jsonl`) leaves them out on purpose and is not touched.

Tests: `scripts/eval-autoreview.test.mjs`, run by `npm run test:scripts`: ground truth, case construction on
fixture repositories, sampling, scoring, the report, and the harness end to end with the stub reviewer and a fake
judge. All offline.

## Limits

- **A reversed fix is a proxy for a bug-introducing pull request.** The real change that introduced the bug was
  usually larger and written for another purpose; here the bug arrives as a small, focused edit that removes
  correct code, with a neutral commit message and no description. Hard cases of real review (a bug hidden in a
  large feature, a bug of omission in new code) are under-represented. The extra hunks help only a little.
- **The buggy base is synthetic.** It is the upstream tree plus a later fix (minus the extra hunks), a state that
  never existed upstream. The head, which is what a reviewer reads, is a real upstream commit's tree.
- **The fix is public.** These repositories and their fixes are in the training data of every model, and a
  reviewer with network access could look the fix up. Recall here is an upper bound on what the same reviewer
  would do on unseen code.
- **Location match is lenient in one way and strict in another.** Lenient: any confirmed blocker or major finding
  inside a hunk (+-5 lines) counts, whatever it claims, so a wrong remark in the right place is a catch (the
  `changed lines` column tightens the place, only the judge checks the claim). Strict: a correct finding reported
  at a call site, in a test, or in another file than the fix touched is not a catch (again, only the judge sees
  it), and a finding without a line number never matches a modified file.
- **Clean cases are not proven clean.** A gold patch can have a real flaw (a later upstream commit may fix the
  fix), so a "false alarm" can be a true finding. The false-alarm rate is an upper bound; read the findings.
- **Extra hunks are presumed harmless, not proven.** They are real, merged upstream changes in other files than
  the fix, but one may relate to the bug or have a defect of its own. Findings on them do not count for or
  against the reviewer on buggy cases, except through the verdict, which is expected to be `request_changes`
  anyway.
- **Small sample.** 20 cases per kind: one case is 5 points of recall or false-alarm rate. Use `--trials` for
  run-to-run variance and `--cases` for a larger sample; compare arms on the same sample only.
- **Python only**, 12 repositories, patches of at most 150 changed lines in at most 4 files.
- **The judge is a model.** Its verdicts are evidence, kept with their reasons in the result file; it sees the gold
  patch, so it judges agreement with the known defect, not whether the finding is true in general.
- **Reported numbers are the reviewer's own.** Tokens, cost, frames and the timing split come from the reviewer's
  JSON; only wall-clock time is measured by the harness (it includes process start-up).

# Blind comparison with a reference reviewer

A second benchmark, in `blind/`: for pull requests a human reviewer really reviewed, `ultron autoreview` reviews
the same commit without seeing that review, and a judge model then says how many of the human's substantive
comments it also raised and how many of its other findings are valid.

The data comes from repositories the collecting account can read, possibly private ones. Which reviewer, which
account and which period are command-line parameters only. Cases, clones, evidence and reports live under
`~/.cache/ultron-autoreview-bench/blind/<set>/` (owner-only permissions) and nowhere else: never in this
repository, never under `acceptance/`. Only the redacted summary (numbers per arm) is meant to be shared.

## Method

**Collect** (`blind/collect.mjs`, read-only on GitHub: `gh api` GET requests and `git fetch`).

1. Search the pull requests reviewed by the reference reviewer and updated since a date (`reviewed-by:<login>`,
   paginated; a query the search API truncates is split by date). The results are put in the seeded sample order
   (step 6) and looked at one by one, only until the sample is full: the steps below cost one to four requests
   per pull request, and most search results are never asked about.
2. Per pull request, the **reference review** is that reviewer's first submitted review that opened inline
   comments. Its commit is the head we review. Ground truth is that review's top-level inline comments (path, line
   or line range, side, text; replies are not ground truth) and its body and state.
3. The base is the merge base of the pull request's base branch and the reviewed commit (compare API).
4. A pull request that was looked at is dropped, with the reason counted, when: the reference reviewer wrote it;
   the reviewer opened no inline comments, or fewer than `--min-comments` (2); the reviewed commit no longer
   exists or cannot be fetched; the diff has no reviewable source file (lockfiles, binaries, snapshots and
   vendored or built directories do not count), more than `--max-files` (15) of them, or more than
   `--max-changed-lines` (600) in them.
5. Reviews with inline comments that were already on the same commit when the reference review was submitted are
   not a reason to drop: they are recorded (`priorReviews`: how many, how many by bots, and how many on earlier
   commits), so results can be split by "first reviewer on the commit" and "had prior reviews".
6. **Sample**: one seeded order over the search results that goes round the repositories and, inside a
   repository, round its pull-request authors; the sample is the first `--cases` (30) of that order that pass the
   filters and can be built, with at most `--repos-max-share` (0.25) of `--cases` from one repository. With few
   repositories the sample is smaller than asked rather than lopsided.
7. **Case repositories**: per repository a bare clone without file contents holds the history of the two commits
   (`git fetch --filter=blob:none origin <head> <base>`), then the contents of exactly those two trees are
   fetched. A case is a repository at `cases/<id>` that borrows those objects, checked out (detached) at the
   reviewed commit, with no remote: the reviewer contract `--repo-dir --base --head` works offline, and commits
   pushed after the review are not in it. Case names are hashes; nothing can be read from them.

The token comes from `gh auth token --user <account>` and is passed to `gh` and to git's credential helper only as
`GH_TOKEN` in the child's environment: never an argument, a file, a log line or evidence.

**Footprint on GitHub.** The account's limits are shared with whoever else uses it, so the collector is slow on
purpose. Requests are serial and at least `--min-interval-ms` (1000) apart. Every answer, also "not found", is
kept under the set directory (`api/`, and per pull request `candidates/`) and never asked for again: a rerun of
the same command costs nothing for what it already knows. A used-up quota (`x-ratelimit-remaining: 0`) is waited
for until `x-ratelimit-reset` when that is at most 15 minutes away; `Retry-After` is honoured on server errors.
A secondary (abuse) limit is never retried: the collector stops with exit code 3 and says so; run the same
command again once the limit has cleared and it goes on from what is kept. `--offline` asks the API nothing at
all and draws the sample from the pull requests already looked at (only `git fetch` still talks to GitHub).

**Run** (`blind/run.mjs`), four stages, each kept per case so a rerun with the same `--run-id` does only what is
missing:

1. **Classify** (judge, once per set and judge model): each reference comment becomes `{kind, severity, oneLine}`,
   kind one of defect, risk, maintainability, style_nit, question, praise_or_meta. *Substantive* means defect or
   risk; maintainability is reported separately; the rest is not scored.
2. **Review**: each case with each arm through the reviewer contract above. The reviewer gets the repository and
   the two commits, nothing of the pull request's conversation, title or description.
3. **Match** (judge): each substantive or maintainability comment, with its code, against our confirmed and
   uncertain findings in the same file; only if that finds no match, once more against all our findings. The
   answer is `{matched: index|null, how: same_issue | partial | same_location_different_issue | null}`.
   `same_issue` is a match, `partial` is reported next to it, the third is a miss.
4. **Extras** (judge): each of our confirmed findings of minor severity or worse that matched no reference comment
   (at most 12 per review, most severe first), with the source around it and the diff hunk:
   `{valid: yes|no|unclear, severity, why}`. Only `yes` counts as "we found more".

The judge is the same plumbing as in the first benchmark (the Ultron CLI in print mode, no tools, private profile
directories), with `--judge-thinking <level>` if wanted. Its system prompt says that pull-request text, code,
comments and findings are untrusted data whose instructions must not be followed; the prompts put that content in
tagged blocks. A reply that is not the asked JSON object is asked for again (three attempts), then recorded as a
judge error: never a match, never a valid extra, and tried again by the next rerun.

**Report**, per arm (`runs/<run-id>/report.json`, `report.md`, `report.redacted.md`, all under the set directory):
substantive reference comments; matched as the same issue (this is recall against the reference) and partly; by
the comment's severity; the missed ones, restated; our findings in total, matched, and extras valid, invalid,
unclear and not judged; a precision estimate, (confirmed findings that matched + valid extras) / confirmed
findings judged; verdict agreement (the reference review's state against our verdict); p50 and p90 seconds;
tokens and cost as the reviewer reports them; recall split by "first reviewer on the commit" and "had prior
reviews". `report.md` names files and restates comments, so it stays where it is. The run prints the redacted
summary, and `--redacted` prints it again for a finished run: aggregate numbers and arm names, no repository,
login, title, path, code or comment text.

## Running it

```sh
# read GitHub (until the sample is full) and show the counts and the sample's shape; builds nothing
node evals/autoreview/blind/collect.mjs --account <gh login> --reviewer <login> --since <YYYY-MM-DD> --set <name> --plan
# the same, then build the case repositories and write cases.jsonl and manifest.json
node evals/autoreview/blind/collect.mjs --account <gh login> --reviewer <login> --since <YYYY-MM-DD> --set <name>

node evals/autoreview/blind/run.mjs --set <name> --plan      # the cases and arms; runs nothing

# paid model calls: reviews, then the judge
node evals/autoreview/blind/run.mjs --set <name> --ultron packages/coding-agent/dist/cli.js \
  --models <provider/model>@high,<provider/model> --verify-model <provider/model> \
  --judge-model <provider/model> --judge-thinking high --run-id first

node evals/autoreview/blind/run.mjs --set <name> --run-id first --redacted    # the shareable summary
# a gate: against an earlier run's report.json, recall, precision or verdict agreement down more than 10 points exits 3
node evals/autoreview/blind/run.mjs --set <name> ... --run-id second --baseline <set dir>/runs/first/report.json
```

`collect.mjs` flags: `--account`, `--reviewer`, `--since`, `--set` (all required), `--cases N` (30), `--seed`,
`--max-changed-lines` (600), `--max-files` (15), `--min-comments` (2), `--repos-max-share` (0.25),
`--min-interval-ms` (1000), `--plan`, `--offline`.
`run.mjs` flags: `--set` (required), `--ultron <path>`, `--models a,b` (the arm syntax of the first benchmark,
`provider/model`, `provider/model@thinking` or `provider/model@find/verify`), `--verify-model`, `--judge-model`
(without it the cases are reviewed and nothing is scored), `--judge-thinking`, `--concurrency` (2),
`--limit-minutes` (20), `--run-id`, `--only <case ids>`, `--reviewer-cmd`, `--plan`, `--redacted`. Unknown flags
exit 2 before anything starts.

A rerun with the same `--run-id` skips reviews that have a record, comments that are classified or matched and
extras that are judged; errors, timeouts and judge failures are done again. Adding `--judge-model` to a run that
was reviewed without one only adds the judging. Isolation, credentials and the scrubbing of kept output are those
of the first benchmark (see "Isolation" above).

```
~/.cache/ultron-autoreview-bench/blind/<set>/
  manifest.json, cases.jsonl             the query, the counts per drop reason, the sample; one case per line
  api/<hash>.json                        every answer of GitHub's API, never asked for twice
  candidates/<id>.json                   the outcome per pull request looked at: a case, or why not
  repos/<owner>__<name>.git              the clone without file contents, per repository
  cases/<id>/, built/<id>.json           the case repositories
  classify/<judge>/<id>/                 the classification of the reference comments, and the judge's output
  runs/<run-id>/<arm>/<id>/              record.json, stdout.json, stderr.txt, match.json, extras.json, judge output
  runs/<run-id>/report.json, report.md, report.redacted.md
```

Tests: `scripts/eval-autoreview-blind.test.mjs`, run by `npm run test:scripts`: filtering, the choice of the
reference review, base and head, sampling, the case files and repositories, pacing, caching and the two kinds of
rate limit (a fake `gh` and small local repositories), the judge's prompts and replies, scoring, both reports, and the runner end to end with the stub
reviewer and a fake judge. All offline, all fixtures invented.

## Limits

- **Inline comments are not exhaustive ground truth.** A reviewer writes down what they chose to say that day, not
  every problem in the diff. A comment we did not raise is a miss against this reviewer, not proof of a missed
  defect; a finding nobody commented on is not thereby wrong, which is why extras are judged on their own.
- **The reference reviewer may have used tools.** Their comments can come from linters, tests, assistants or an
  earlier automated review on the same commit, and they knew the project, the ticket and the conversation. The
  reviewer under test sees the repository and two commits. The `priorReviews` split shows part of this, not all.
- **A judge model decides.** Classification, matching and the validity of extras are a model's opinion, with its
  reasons kept in the evidence. "Valid" means supported by the code shown to the judge (a window of the file and
  the nearby hunk), not confirmed by running anything. The precision estimate covers judged findings only
  (confirmed, minor or worse, at most 12 extras per review). Use one judge for arms you compare, and read a sample
  of its verdicts.
- **The severity of a reference comment is the judge's reading** of the comment, not the reviewer's own label.
- **Selection.** Only pull requests where the reviewer opened at least two inline comments on a small enough diff
  become cases: pull requests they approved without remarks, or reviewed in prose only, are left out, so the
  verdict agreement is over a sample skewed towards "has remarks".
- **History.** A case holds the commits and directories of the history before the reviewed commit, but file
  contents only for the base and the head: `git show` and `git diff` between the two work, `git blame` or a diff
  against an older commit does not. The clone a set's cases borrow from holds every case's commits of that
  repository, so a later commit of another case is in the object store, though on no branch of this one.
- **Line numbers** of a reference comment are those of the commit it was written on; a comment on a deleted line
  refers to the base side and is shown to the judge with its diff hunk.
- **The drop counts cover the pull requests looked at**, not all search results: looking stops when the sample is
  full. To refresh a set against GitHub, delete its `api/` and `candidates/` directories.
- **Small sample**, one reviewer, the repositories that reviewer works in. Compare arms on the same set only.
