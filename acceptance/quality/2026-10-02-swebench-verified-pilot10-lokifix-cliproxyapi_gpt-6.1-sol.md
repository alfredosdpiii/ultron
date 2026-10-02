# SWE-bench Verified, 10 tasks, cliproxyapi/gpt-6.1-sol

Run `pilot10-lokifix`, 2026-10-02. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 9/10 | 1 | 0 | 0 | 0 | 10m59s | 68 | 58 | 100,256 / 328,192 / 11,194 | $0.35 |

Cost is notional: tokens at the model's list prices from models.json. The proxy bills a subscription.

## Per task

| instance | ultron: resolved, time, turns, tokens |
| --- | --- |
| django__django-16667 | yes, 1m03s, 7, 41,045 |
| django__django-14855 | yes, 0m54s, 6, 30,364 |
| astropy__astropy-7166 | yes, 0m48s, 5, 20,240 |
| django__django-16485 | yes, 0m42s, 5, 24,163 |
| sympy__sympy-12419 | yes, 2m24s, 13, 120,521 |
| django__django-14771 | no, 1m00s, 6, 28,687 |
| django__django-13363 | yes, 0m53s, 5, 32,303 |
| sphinx-doc__sphinx-8638 | yes, 0m58s, 8, 68,439 |
| django__django-13516 | yes, 1m08s, 8, 43,740 |
| pydata__xarray-3095 | yes, 1m09s, 5, 30,140 |

## Harness failures, timeouts and evaluation errors

None: every run ended by itself and every prediction was evaluated.

## Checks

- ultron: verified on gpt-6.1-sol via /v1/chat/completions, reasoning effort medium (2 requests seen by the recorder)

## How Ultron worked

| instance | cells | failed cells | helpers awaited | frame calls | sub-agent sessions | Loki: write checks / blocked / cells with findings |
| --- | --- | --- | --- | --- | --- | --- |
| django__django-16667 | 6 | 0 | bash 6, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14855 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| astropy__astropy-7166 | 4 | 0 | bash 4, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-16485 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-12419 | 12 | 0 | bash 11, edit 4, background.jobs 1, rlm.job 1, job.result 1 | 0 | 0 | 4 / 0 / 0 |
| django__django-14771 | 5 | 0 | bash 5, edit 1, background.result 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13363 | 4 | 0 | bash 4, read 1, write 1 | 0 | 0 | 1 / 0 / 0 |
| sphinx-doc__sphinx-8638 | 7 | 0 | bash 6, edit 1, rlm.job 1, j.result 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13516 | 7 | 1 | bash 5, edit 1, background.get 1, rlm.job 1 | 0 | 0 | 1 / 0 / 0 |
| pydata__xarray-3095 | 4 | 0 | bash 4, edit 2 | 0 | 0 | 2 / 0 / 0 |
