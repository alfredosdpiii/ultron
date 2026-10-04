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
(default `acceptance/quality`), `--plan`, `--build-only`. Unknown flags exit 2 before anything starts.

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
