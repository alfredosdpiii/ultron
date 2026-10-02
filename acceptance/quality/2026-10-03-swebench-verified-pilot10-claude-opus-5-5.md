# SWE-bench Verified, 10 tasks, claude-code/claude-opus-5-5

Run `pilot10-opus`, 2026-10-02. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cache read / cache write / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| claude | 9/10 | 1 | 0 | 0 | 0 | 7m15s | 61 | 58 | 122 / 1,101,050 / 86,177 / 23,602 | $1.38 |
| ultron-claude | 9/10 | 1 | 0 | 0 | 0 | 7m52s | 49 | 40 | 98 / 277,640 / 35,337 / 15,352 | $0.65 |

Cost is notional: what Claude Code reports for each run (total_cost_usd, its tokens at list prices); the runs were made on a Claude subscription, which is not billed per token.

## Per task

| instance | claude: resolved, time, turns, tokens, cost | ultron-claude: resolved, time, turns, tokens, cost |
| --- | --- | --- |
| django__django-16667 | no, 0m13s, 3, 52,021, $0.07 | no, 0m12s, 3, 15,311, $0.03 |
| django__django-14855 | yes, 0m25s, 4, 69,323, $0.08 | yes, 0m22s, 3, 14,847, $0.03 |
| astropy__astropy-7166 | yes, 0m14s, 3, 50,840, $0.07 | yes, 0m14s, 3, 13,280, $0.03 |
| django__django-16485 | yes, 0m26s, 5, 89,569, $0.11 | yes, 0m17s, 3, 13,771, $0.04 |
| sympy__sympy-12419 | yes, 3m46s, 14, 342,514, $0.45 | yes, 3m24s, 12, 111,548, $0.22 |
| django__django-14771 | yes, 0m16s, 4, 67,376, $0.09 | yes, 0m12s, 3, 14,311, $0.03 |
| django__django-13363 | yes, 0m32s, 9, 170,295, $0.15 | yes, 0m17s, 5, 28,791, $0.05 |
| sphinx-doc__sphinx-8638 | yes, 0m42s, 11, 228,483, $0.20 | yes, 0m54s, 8, 65,832, $0.12 |
| django__django-13516 | yes, 0m17s, 4, 68,419, $0.08 | yes, 0m44s, 4, 20,321, $0.04 |
| pydata__xarray-3095 | yes, 0m23s, 4, 72,111, $0.09 | yes, 1m16s, 5, 30,415, $0.06 |

## Harness failures, timeouts and evaluation errors

None: every run ended by itself and every prediction was evaluated.

## Checks

- claude: verified on claude-opus-5-5 via claude -p, reasoning effort medium (2 responses in Claude Code's session transcript)
- ultron-claude: verified on claude-opus-5-5 via ultron --claude (claude -p), reasoning effort medium (2 responses in Claude Code's session transcript)

## Claude Code

- claude: model claude-opus-5-5, effort medium (Claude Code's session transcripts, 61 responses); tool calls denied: none
- ultron-claude: model claude-opus-5-5, effort medium (Claude Code's session transcripts, 49 responses); tool calls denied: none

Subscription windows as Claude Code reported them (rate_limit events), first run to last: five_hour 75% to 77%, seven_day 20% to 20%. The windows are the account's and are shared with everything else running on it, so the difference is an upper bound on what these runs used, and means nothing across a window's reset.

## How Ultron worked

| instance | cells | failed cells | helpers awaited | frame calls | sub-agent sessions | Loki: write checks / blocked / cells with findings |
| --- | --- | --- | --- | --- | --- | --- |
| django__django-16667 | 2 | 0 | bash 2, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14855 | 2 | 0 | bash 2, edit 1 | 0 | 0 | 1 / 0 / 0 |
| astropy__astropy-7166 | 2 | 0 | bash 2, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-16485 | 2 | 0 | bash 2, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-12419 | 12 | 1 | bash 11, edit 6, write 1, _.job.result 1, background.get 1 | 0 | 0 | 7 / 0 / 0 |
| django__django-14771 | 2 | 0 | bash 2, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13363 | 4 | 0 | bash 4, read 1, write 1 | 0 | 0 | 1 / 0 / 0 |
| sphinx-doc__sphinx-8638 | 7 | 0 | bash 7, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-13516 | 3 | 0 | bash 3, edit 1 | 0 | 0 | 1 / 0 / 0 |
| pydata__xarray-3095 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
