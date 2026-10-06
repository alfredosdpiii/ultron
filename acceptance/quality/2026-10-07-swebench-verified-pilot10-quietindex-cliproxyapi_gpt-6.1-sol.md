# SWE-bench Verified, 10 tasks, cliproxyapi/gpt-6.1-sol

Run `pilot10-quietindex`, 2026-10-06. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 9/10 | 1 | 0 | 0 | 0 | 7m05s | 68 | 58 | 110,473 / 448,256 / 12,480 | $0.39 |

Cost is notional: tokens at the model's list prices from models.json. The proxy bills a subscription.

## Per task

| instance | ultron: resolved, time, turns, tokens, cost |
| --- | --- |
| django__django-16667 | yes, 0m33s, 6, 31,759, $0.03 |
| django__django-14855 | yes, 0m37s, 6, 36,060, $0.03 |
| astropy__astropy-7166 | yes, 0m32s, 5, 23,189, $0.02 |
| django__django-16485 | yes, 0m28s, 5, 28,219, $0.02 |
| sympy__sympy-12419 | yes, 2m06s, 18, 256,945, $0.13 |
| django__django-14771 | no, 0m38s, 6, 33,434, $0.03 |
| django__django-13363 | yes, 0m27s, 4, 25,304, $0.02 |
| sphinx-doc__sphinx-8638 | yes, 0m45s, 9, 85,208, $0.05 |
| django__django-13516 | yes, 0m24s, 4, 18,374, $0.02 |
| pydata__xarray-3095 | yes, 0m35s, 5, 32,717, $0.03 |

## Harness failures, timeouts and evaluation errors

None: every run ended by itself and every prediction was evaluated.

## Checks

- ultron: verified on gpt-6.1-sol via /v1/chat/completions, reasoning effort medium (2 requests seen by the recorder)

## How Ultron worked

| instance | cells | failed cells | helpers awaited | frame calls | sub-agent sessions | Loki: write checks / blocked / cells with findings |
| --- | --- | --- | --- | --- | --- | --- |
| django__django-16667 | 5 | 0 | bash 5, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-14855 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| astropy__astropy-7166 | 4 | 0 | bash 4, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-16485 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-12419 | 17 | 1 | bash 14, edit 5, background.get 1, rlm.job 3, job.result 1, j2.result 1 | 0 | 0 | 5 / 0 / 0 |
| django__django-14771 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13363 | 3 | 0 | bash 4, read 1, write 1 | 0 | 0 | 1 / 0 / 0 |
| sphinx-doc__sphinx-8638 | 8 | 0 | bash 7, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13516 | 3 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| pydata__xarray-3095 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
