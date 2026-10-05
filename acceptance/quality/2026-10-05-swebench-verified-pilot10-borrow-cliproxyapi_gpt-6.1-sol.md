# SWE-bench Verified, 10 tasks, cliproxyapi/gpt-6.1-sol

Run `pilot10-borrow`, 2026-10-05. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 8/10 | 2 | 0 | 0 | 0 | 10m18s | 62 | 52 | 104,703 / 311,680 / 10,277 | $0.34 |

Cost is notional: tokens at the model's list prices from models.json. The proxy bills a subscription.

## Per task

| instance | ultron: resolved, time, turns, tokens, cost |
| --- | --- |
| django__django-16667 | no, 0m39s, 5, 26,481, $0.03 |
| django__django-14855 | yes, 0m39s, 5, 26,093, $0.02 |
| astropy__astropy-7166 | yes, 0m59s, 5, 19,699, $0.02 |
| django__django-16485 | yes, 0m47s, 5, 25,041, $0.02 |
| sympy__sympy-12419 | yes, 2m15s, 13, 139,847, $0.09 |
| django__django-14771 | no, 0m52s, 5, 20,745, $0.03 |
| django__django-13363 | yes, 0m48s, 4, 23,837, $0.03 |
| sphinx-doc__sphinx-8638 | yes, 1m34s, 7, 67,532, $0.05 |
| django__django-13516 | yes, 0m36s, 5, 21,650, $0.02 |
| pydata__xarray-3095 | yes, 1m10s, 8, 55,735, $0.05 |

## Harness failures, timeouts and evaluation errors

None: every run ended by itself and every prediction was evaluated.

## Checks

- ultron: verified on gpt-6.1-sol via /v1/chat/completions, reasoning effort medium (2 requests seen by the recorder)

## How Ultron worked

| instance | cells | failed cells | helpers awaited | frame calls | sub-agent sessions | Loki: write checks / blocked / cells with findings |
| --- | --- | --- | --- | --- | --- | --- |
| django__django-16667 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14855 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| astropy__astropy-7166 | 4 | 0 | bash 4, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-16485 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-12419 | 12 | 0 | bash 11, edit 2, __import__ 1, testjob.job.result 1, rlm.job 1, job.result 1 | 0 | 0 | 2 / 0 / 0 |
| django__django-14771 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13363 | 3 | 0 | bash 3, edit 2 | 0 | 0 | 4 / 0 / 0 |
| sphinx-doc__sphinx-8638 | 6 | 0 | bash 10, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13516 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| pydata__xarray-3095 | 7 | 0 | bash 6, edit 1, rlm.job 1 | 0 | 0 | 1 / 0 / 0 |
