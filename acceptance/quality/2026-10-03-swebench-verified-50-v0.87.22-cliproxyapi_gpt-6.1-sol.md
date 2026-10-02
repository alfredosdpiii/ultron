# SWE-bench Verified, 50 tasks, cliproxyapi/gpt-6.1-sol

Run `pilot10-v08722`, 2026-10-02. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 44/50 | 6 | 0 | 0 | 0 | 71m49s | 358 | 308 | 745,301 / 1,873,664 / 69,282 | $2.37 |
| codex | 44/50 | 6 | 0 | 0 | 0 | 99m59s | 483 | 347 | 1,278,951 / 8,057,088 / 96,852 | $4.33 |
| pi | 43/50 | 7 | 0 | 0 | 0 | 82m44s | 459 | 476 | 952,806 / 2,130,560 / 72,214 | $2.84 |

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
| sympy__sympy-21612 | yes, 2m31s, 12, 118,837 | yes, 2m41s, 14, 307,470 | yes, 2m33s, 14, 105,841 |
| django__django-11333 | yes, 0m53s, 5, 25,463 | yes, 1m10s, 6, 97,396 | yes, 0m53s, 7, 29,875 |
| scikit-learn__scikit-learn-13779 | yes, 0m58s, 6, 24,376 | yes, 1m11s, 7, 109,592 | yes, 0m53s, 7, 22,828 |
| matplotlib__matplotlib-26208 | yes, 1m43s, 10, 82,176 | yes, 3m01s, 16, 376,826 | yes, 1m47s, 11, 84,468 |
| pytest-dev__pytest-6197 | yes, 1m54s, 12, 126,401 | yes, 1m47s, 12, 245,231 | yes, 2m21s, 13, 151,573 |
| django__django-12965 | yes, 1m06s, 5, 34,511 | yes, 1m46s, 9, 166,963 | yes, 1m22s, 8, 44,779 |
| django__django-11999 | yes, 1m03s, 6, 31,328 | yes, 1m23s, 7, 111,030 | yes, 0m59s, 6, 30,265 |
| sympy__sympy-19495 | yes, 1m26s, 7, 52,945 | yes, 1m56s, 10, 181,496 | yes, 1m39s, 9, 45,673 |
| django__django-15368 | yes, 1m02s, 5, 25,330 | yes, 1m13s, 8, 124,434 | yes, 0m59s, 6, 28,402 |
| django__django-14007 | yes, 1m31s, 6, 29,061 | yes, 1m55s, 8, 138,525 | yes, 1m11s, 8, 29,942 |
| sphinx-doc__sphinx-9230 | yes, 1m12s, 7, 53,070 | yes, 1m48s, 11, 214,811 | yes, 1m00s, 7, 35,920 |
| psf__requests-1142 | yes, 1m05s, 6, 28,644 | yes, 1m59s, 8, 129,525 | yes, 1m59s, 9, 42,919 |
| django__django-14011 | yes, 1m13s, 6, 50,533 | yes, 1m34s, 8, 143,298 | yes, 1m38s, 9, 56,504 |
| astropy__astropy-14182 | yes, 1m00s, 5, 31,830 | yes, 1m36s, 10, 202,327 | yes, 1m33s, 10, 74,663 |
| sympy__sympy-12096 | yes, 2m06s, 11, 78,491 | yes, 3m14s, 14, 298,027 | yes, 1m45s, 9, 48,335 |
| django__django-13964 | yes, 1m13s, 6, 32,834 | yes, 1m44s, 8, 134,417 | yes, 1m12s, 7, 33,156 |
| django__django-11206 | yes, 1m18s, 5, 26,682 | yes, 1m15s, 6, 95,837 | yes, 1m22s, 7, 27,842 |
| matplotlib__matplotlib-23476 | yes, 1m32s, 7, 54,028 | yes, 2m31s, 10, 185,698 | yes, 2m35s, 9, 53,103 |
| scikit-learn__scikit-learn-13328 | yes, 0m42s, 4, 21,546 | yes, 1m02s, 6, 97,981 | yes, 0m51s, 7, 26,526 |
| django__django-14155 | no, 1m00s, 5, 24,834 | no, 1m09s, 7, 115,878 | no, 0m55s, 6, 24,377 |
| pylint-dev__pylint-4551 | no, 2m17s, 7, 59,752 | no, 3m17s, 12, 247,628 | no, 3m17s, 13, 206,945 |
| django__django-11555 | yes, 3m02s, 10, 93,831 | yes, 1m52s, 9, 159,856 | yes, 2m29s, 13, 80,459 |
| sympy__sympy-24213 | yes, 1m11s, 6, 29,191 | yes, 1m05s, 6, 92,867 | yes, 1m03s, 7, 32,334 |
| sphinx-doc__sphinx-7748 | no, 2m52s, 11, 133,835 | no, 5m29s, 17, 417,614 | no, 2m57s, 13, 107,667 |
| pydata__xarray-6721 | yes, 1m22s, 6, 43,091 | yes, 2m50s, 15, 321,344 | yes, 1m29s, 9, 65,691 |
| django__django-14559 | yes, 1m09s, 5, 25,643 | yes, 1m22s, 7, 113,165 | yes, 1m15s, 7, 34,616 |
| django__django-12858 | yes, 1m24s, 6, 31,598 | yes, 1m26s, 7, 112,293 | yes, 1m02s, 8, 29,604 |
| django__django-14053 | yes, 1m58s, 8, 53,483 | yes, 1m46s, 8, 148,855 | yes, 1m49s, 9, 63,192 |
| sympy__sympy-16597 | no, 1m46s, 10, 68,115 | no, 5m13s, 31, 724,600 | no, 2m06s, 10, 62,365 |
| pytest-dev__pytest-5787 | yes, 2m04s, 9, 84,677 | yes, 2m29s, 9, 174,151 | yes, 2m14s, 12, 81,684 |
| django__django-11728 | yes, 1m11s, 6, 26,055 | yes, 1m43s, 7, 111,376 | yes, 1m16s, 8, 37,579 |
| matplotlib__matplotlib-23314 | yes, 1m42s, 11, 66,101 | yes, 1m35s, 9, 136,836 | yes, 1m13s, 7, 24,659 |
| django__django-14351 | yes, 1m43s, 7, 77,602 | yes, 2m08s, 9, 185,934 | yes, 2m52s, 14, 167,833 |
| sphinx-doc__sphinx-9673 | yes, 1m17s, 7, 45,192 | yes, 1m11s, 7, 115,970 | yes, 1m05s, 8, 47,714 |
| scikit-learn__scikit-learn-26194 | yes, 1m06s, 5, 30,458 | yes, 1m52s, 8, 150,119 | yes, 1m20s, 8, 42,664 |
| django__django-14999 | yes, 0m55s, 5, 28,555 | yes, 2m04s, 8, 136,398 | yes, 3m49s, 18, 158,621 |
| sympy__sympy-21847 | yes, 0m52s, 5, 33,809 | yes, 1m09s, 7, 118,685 | yes, 0m49s, 6, 36,356 |
| django__django-16263 | no, 2m53s, 11, 116,763 | no, 5m08s, 15, 362,133 | no, 5m06s, 22, 252,013 |
| astropy__astropy-7606 | yes, 0m41s, 5, 18,363 | yes, 1m22s, 9, 142,859 | yes, 0m48s, 7, 23,421 |
| django__django-14122 | yes, 0m47s, 6, 34,009 | yes, 1m16s, 8, 130,641 | yes, 1m20s, 9, 45,348 |

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
| sympy__sympy-21612 | 11 | 0 | bash 11, edit 4, rlm.job 1, job.result 1 | 0 | 0 | 4 / 0 / 0 |
| django__django-11333 | 4 | 0 | bash 4, edit 3 | 0 | 0 | 3 / 0 / 0 |
| scikit-learn__scikit-learn-13779 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| matplotlib__matplotlib-26208 | 9 | 0 | bash 8, edit 1, tests.job.result 1 | 0 | 0 | 2 / 0 / 0 |
| pytest-dev__pytest-6197 | 11 | 0 | bash 11, edit 1, write 2, rlm.job 3, job1.result 1, job2.result 1, job3.result 1 | 0 | 0 | 3 / 0 / 1 |
| django__django-12965 | 4 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-11999 | 5 | 0 | bash 4, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-19495 | 6 | 0 | bash 5, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-15368 | 4 | 0 | bash 4, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-14007 | 5 | 0 | bash 6, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| sphinx-doc__sphinx-9230 | 6 | 0 | bash 6, edit 1 | 0 | 0 | 1 / 0 / 0 |
| psf__requests-1142 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14011 | 5 | 0 | bash 5, edit 3 | 0 | 0 | 3 / 0 / 0 |
| astropy__astropy-14182 | 4 | 0 | bash 4, edit 3 | 0 | 0 | 3 / 0 / 0 |
| sympy__sympy-12096 | 10 | 1 | bash 8, edit 2, background.get 1, rlm.job 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-13964 | 5 | 0 | bash 5, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-11206 | 4 | 0 | bash 4, edit 3 | 0 | 0 | 3 / 0 / 0 |
| matplotlib__matplotlib-23476 | 6 | 0 | bash 6, edit 1 | 0 | 0 | 1 / 0 / 0 |
| scikit-learn__scikit-learn-13328 | 3 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14155 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| pylint-dev__pylint-4551 | 6 | 0 | bash 6, read 1, edit 6 | 0 | 0 | 6 / 0 / 0 |
| django__django-11555 | 9 | 0 | bash 9, edit 7, write 1 | 0 | 0 | 8 / 0 / 0 |
| sympy__sympy-24213 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sphinx-doc__sphinx-7748 | 10 | 0 | bash 9, read 1, edit 6, rlm.job 1, job.result 1 | 0 | 0 | 6 / 0 / 0 |
| pydata__xarray-6721 | 5 | 0 | bash 6, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-14559 | 4 | 0 | bash 5, edit 7 | 0 | 0 | 7 / 0 / 0 |
| django__django-12858 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14053 | 7 | 0 | bash 6, edit 3, rlm.job 1, job.result 1 | 0 | 0 | 3 / 0 / 0 |
| sympy__sympy-16597 | 9 | 0 | bash 9, edit 1, suite.job.result 1 | 0 | 0 | 1 / 0 / 0 |
| pytest-dev__pytest-5787 | 8 | 0 | bash 7, read 2, write 1, edit 2 | 0 | 0 | 3 / 0 / 1 |
| django__django-11728 | 5 | 0 | bash 5, edit 4 | 0 | 0 | 4 / 0 / 0 |
| matplotlib__matplotlib-23314 | 10 | 1 | bash 5, edit 1, background.get 1, rlm.job 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14351 | 6 | 0 | bash 6, edit 5 | 0 | 0 | 5 / 0 / 0 |
| sphinx-doc__sphinx-9673 | 6 | 0 | bash 6, edit 1 | 0 | 0 | 1 / 0 / 0 |
| scikit-learn__scikit-learn-26194 | 4 | 0 | bash 4, edit 3 | 0 | 0 | 3 / 0 / 0 |
| django__django-14999 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-21847 | 4 | 0 | bash 4, edit 3 | 0 | 0 | 3 / 0 / 0 |
| django__django-16263 | 10 | 0 | bash 9, edit 8, tools.jobs.result 1, rlm.job 2 | 0 | 0 | 8 / 0 / 0 |
| astropy__astropy-7606 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14122 | 5 | 0 | bash 5, edit 2, rlm.job 1, job.result 1 | 0 | 0 | 2 / 0 / 0 |
