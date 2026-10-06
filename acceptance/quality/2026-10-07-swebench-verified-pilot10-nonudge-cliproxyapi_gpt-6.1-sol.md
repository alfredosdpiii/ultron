# SWE-bench Verified, 10 tasks, cliproxyapi/gpt-6.1-sol

Run `pilot10-nonudge`, 2026-10-06. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 8/10 | 2 | 0 | 0 | 0 | 10m11s | 84 | 74 | 147,623 / 904,704 / 15,786 | $0.54 |

Cost is notional: tokens at the model's list prices from models.json. The proxy bills a subscription.

## Per task

| instance | ultron: resolved, time, turns, tokens, cost |
| --- | --- |
| django__django-16667 | no, 0m34s, 5, 43,465, $0.05 |
| django__django-14855 | yes, 0m36s, 6, 57,939, $0.04 |
| astropy__astropy-7166 | yes, 0m49s, 7, 60,377, $0.04 |
| django__django-16485 | yes, 0m35s, 5, 46,033, $0.03 |
| sympy__sympy-12419 | yes, 2m30s, 20, 400,125, $0.14 |
| django__django-14771 | no, 0m37s, 7, 65,367, $0.04 |
| django__django-13363 | yes, 0m30s, 4, 34,740, $0.03 |
| sphinx-doc__sphinx-8638 | yes, 1m37s, 10, 137,510, $0.07 |
| django__django-13516 | yes, 1m18s, 11, 111,870, $0.05 |
| pydata__xarray-3095 | yes, 1m06s, 9, 110,687, $0.06 |

## Harness failures, timeouts and evaluation errors

None: every run ended by itself and every prediction was evaluated.

## Checks

- ultron: verified on gpt-6.1-sol via /v1/chat/completions, reasoning effort medium (2 requests seen by the recorder)

## How Ultron worked

| instance | cells | failed cells | helpers awaited | frame calls | sub-agent sessions | Loki: write checks / blocked / cells with findings |
| --- | --- | --- | --- | --- | --- | --- |
| django__django-16667 | 4 | 0 | read 1, bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14855 | 5 | 0 | read 1, bash 7, edit 1 | 0 | 0 | 1 / 0 / 0 |
| astropy__astropy-7166 | 6 | 0 | bash 6, read 1, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-16485 | 4 | 0 | read 2, bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-12419 | 19 | 0 | read 1, bash 17, edit 4, rlm.job 4, job.result 2 | 0 | 0 | 4 / 0 / 0 |
| django__django-14771 | 6 | 0 | bash 7, read 1, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13363 | 3 | 0 | read 2, bash 4, edit 1 | 0 | 0 | 2 / 0 / 0 |
| sphinx-doc__sphinx-8638 | 9 | 0 | bash 9, write 1, edit 4 | 0 | 0 | 5 / 0 / 0 |
| django__django-13516 | 10 | 1 | read 1, bash 9, edit 1, background.get 1, rlm.job 3, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| pydata__xarray-3095 | 8 | 1 | read 1, bash 6, edit 1, background.get 1, rlm.job 1 | 0 | 0 | 1 / 0 / 0 |
