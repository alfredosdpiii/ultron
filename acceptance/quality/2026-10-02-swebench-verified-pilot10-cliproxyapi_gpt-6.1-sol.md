# SWE-bench Verified, 10 tasks, cliproxyapi/gpt-6.1-sol

Run `pilot10`, 2026-10-02. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 8/10 | 2 | 0 | 0 | 0 | 10m41s | 65 | 55 | 92,260 / 353,280 / 12,095 | $0.34 |
| codex | 9/10 | 1 | 0 | 0 | 0 | 14m20s | 81 | 61 | 267,714 / 1,227,392 / 17,370 | $0.83 |
| pi | 9/10 | 1 | 0 | 0 | 0 | 11m05s | 79 | 75 | 130,592 / 335,104 / 10,979 | $0.40 |

Cost is notional: tokens at the model's list prices from models.json. The proxy bills a subscription.

## Per task

| instance | ultron: resolved, time, turns, tokens | codex: resolved, time, turns, tokens | pi: resolved, time, turns, tokens |
| --- | --- | --- | --- |
| django__django-16667 | no, 0m41s, 5, 22,276 | yes, 1m01s, 7, 115,127 | yes, 0m56s, 7, 29,155 |
| django__django-14855 | yes, 0m51s, 6, 38,839 | yes, 1m25s, 8, 141,380 | yes, 1m11s, 8, 32,506 |
| astropy__astropy-7166 | yes, 0m46s, 5, 21,198 | yes, 0m46s, 5, 71,199 | yes, 0m43s, 6, 21,489 |
| django__django-16485 | yes, 0m45s, 5, 25,224 | yes, 0m50s, 6, 93,990 | yes, 0m41s, 6, 30,154 |
| sympy__sympy-12419 | yes, 2m33s, 13, 156,032 | yes, 3m14s, 14, 343,109 | yes, 3m11s, 17, 158,437 |
| django__django-14771 | no, 1m10s, 6, 29,516 | no, 0m52s, 5, 72,443 | no, 0m47s, 6, 23,975 |
| django__django-13363 | yes, 0m56s, 5, 30,877 | yes, 1m20s, 7, 122,003 | yes, 1m00s, 8, 44,430 |
| sphinx-doc__sphinx-8638 | yes, 1m03s, 7, 61,390 | yes, 2m35s, 14, 303,932 | yes, 1m05s, 9, 84,318 |
| django__django-13516 | yes, 0m57s, 6, 28,051 | yes, 0m53s, 5, 70,640 | yes, 0m34s, 5, 15,289 |
| pydata__xarray-3095 | yes, 0m59s, 7, 44,232 | yes, 1m24s, 10, 178,653 | yes, 0m56s, 7, 36,922 |

## Where the arms differ

- only codex (not ultron): django__django-16667
- only pi (not ultron): django__django-16667

## Harness failures, timeouts and evaluation errors

None: every run ended by itself and every prediction was evaluated.

## Checks

- ultron: verified on gpt-6.1-sol via /v1/chat/completions, reasoning effort medium (2 requests seen by the recorder)
- codex: verified on gpt-6.1-sol via /v1/responses, reasoning effort medium (2 requests seen by the recorder)
- pi: verified on gpt-6.1-sol via /v1/chat/completions, reasoning effort medium (2 requests seen by the recorder)
- gold patches of the same tasks: 10/10 resolved by the same evaluation

## How Ultron worked

| instance | cells | failed cells | helpers awaited | frame calls | sub-agent sessions | Loki: write checks / blocked / cells with findings |
| --- | --- | --- | --- | --- | --- | --- |
| django__django-16667 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 1 |
| django__django-14855 | 5 | 0 | bash 5, edit 1, background.get 1 | 0 | 0 | 1 / 0 / 1 |
| astropy__astropy-7166 | 4 | 0 | bash 4, edit 2 | 0 | 0 | 2 / 0 / 1 |
| django__django-16485 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 1 |
| sympy__sympy-12419 | 12 | 0 | bash 11, edit 8, read 1, rlm.job 1, job.result 1 | 0 | 0 | 8 / 0 / 2 |
| django__django-14771 | 5 | 0 | bash 5, edit 3 | 0 | 0 | 3 / 0 / 2 |
| django__django-13363 | 4 | 0 | bash 4, read 1, write 1, edit 2 | 0 | 0 | 3 / 0 / 1 |
| sphinx-doc__sphinx-8638 | 6 | 0 | bash 6, edit 1 | 0 | 0 | 1 / 0 / 1 |
| django__django-13516 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 1 |
| pydata__xarray-3095 | 6 | 0 | bash 6, edit 1 | 0 | 0 | 1 / 0 / 1 |
