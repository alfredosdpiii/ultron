# SWE-bench Verified, 10 tasks, cliproxyapi/gpt-6.1-sol

Run `pilot10-v08722`, 2026-10-02. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 9/10 | 1 | 0 | 0 | 0 | 13m06s | 76 | 66 | 126,155 / 496,384 / 12,665 | $0.43 |
| codex | 9/10 | 1 | 0 | 0 | 0 | 17m47s | 90 | 69 | 224,705 / 1,609,728 / 18,372 | $0.79 |
| pi | 8/10 | 2 | 0 | 0 | 0 | 13m53s | 82 | 82 | 121,792 / 424,064 / 11,968 | $0.41 |

Cost is notional: tokens at the model's list prices from models.json. The proxy bills a subscription.

## Per task

| instance | ultron: resolved, time, turns, tokens | codex: resolved, time, turns, tokens | pi: resolved, time, turns, tokens |
| --- | --- | --- | --- |
| django__django-16667 | yes, 0m58s, 6, 30,821 | yes, 1m13s, 7, 108,964 | no, 0m51s, 7, 22,116 |
| django__django-14855 | yes, 0m50s, 7, 42,760 | yes, 1m12s, 7, 111,801 | yes, 1m01s, 8, 29,876 |
| astropy__astropy-7166 | yes, 0m46s, 5, 19,946 | yes, 0m51s, 5, 70,215 | yes, 0m56s, 6, 21,934 |
| django__django-16485 | yes, 0m52s, 5, 29,612 | yes, 0m57s, 5, 74,625 | yes, 0m46s, 6, 28,568 |
| sympy__sympy-12419 | yes, 4m07s, 21, 317,618 | yes, 6m42s, 26, 777,528 | yes, 3m59s, 18, 246,408 |
| django__django-14771 | no, 0m56s, 5, 22,635 | no, 1m21s, 6, 93,105 | no, 1m02s, 6, 23,408 |
| django__django-13363 | yes, 0m52s, 5, 30,137 | yes, 1m25s, 7, 124,504 | yes, 1m37s, 10, 57,891 |
| sphinx-doc__sphinx-8638 | yes, 0m57s, 7, 61,316 | yes, 1m50s, 12, 240,475 | yes, 1m27s, 8, 67,777 |
| django__django-13516 | yes, 1m43s, 9, 46,566 | yes, 1m01s, 7, 106,873 | yes, 1m16s, 7, 28,301 |
| pydata__xarray-3095 | yes, 1m05s, 6, 33,793 | yes, 1m18s, 8, 144,715 | yes, 1m00s, 6, 31,545 |

## Where the arms differ

- only ultron (not pi): django__django-16667
- only codex (not pi): django__django-16667

## Harness failures, timeouts and evaluation errors

None: every run ended by itself and every prediction was evaluated.

## Checks

- ultron: verified on gpt-6.1-sol via /v1/chat/completions, reasoning effort medium (2 requests seen by the recorder)
- codex: verified on gpt-6.1-sol via /v1/responses, reasoning effort medium (2 requests seen by the recorder)
- pi: verified on gpt-6.1-sol via /v1/chat/completions, reasoning effort medium (2 requests seen by the recorder)

## How Ultron worked

| instance | cells | failed cells | helpers awaited | frame calls | sub-agent sessions | Loki: write checks / blocked / cells with findings |
| --- | --- | --- | --- | --- | --- | --- |
| django__django-16667 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14855 | 6 | 1 | bash 5, edit 1, background.get 1 | 0 | 0 | 1 / 0 / 0 |
| astropy__astropy-7166 | 4 | 0 | bash 4, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-16485 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-12419 | 20 | 0 | bash 16, edit 4, rlm.job 5, job.result 1 | 0 | 0 | 4 / 0 / 0 |
| django__django-14771 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13363 | 4 | 0 | bash 4, read 1, write 1, background.get 1, rlm.job 1 | 0 | 0 | 1 / 0 / 0 |
| sphinx-doc__sphinx-8638 | 6 | 0 | bash 6, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-13516 | 8 | 1 | bash 5, edit 1, background.get 1, rlm.job 1 | 0 | 0 | 1 / 0 / 0 |
| pydata__xarray-3095 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
