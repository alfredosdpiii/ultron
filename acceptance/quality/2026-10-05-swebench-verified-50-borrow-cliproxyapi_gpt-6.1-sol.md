# SWE-bench Verified, 50 tasks, cliproxyapi/gpt-6.1-sol

Run `pilot10-borrow`, 2026-10-05. Seed `ultron-swebench-1`, one run per task and arm, 30 min wall-clock limit, 2 tasks at a time. Scored by the official SWE-bench evaluation (swebench 5.0.2) in the prebuilt instance images.

| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 42/50 | 8 | 0 | 0 | 0 | 60m24s | 359 | 308 | 646,741 / 1,966,080 / 73,247 | $2.22 |

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
| sympy__sympy-21612 | yes, 1m47s, 12, 111,908, $0.06 |
| django__django-11333 | yes, 0m53s, 7, 38,536, $0.03 |
| scikit-learn__scikit-learn-13779 | yes, 0m42s, 5, 22,629, $0.02 |
| matplotlib__matplotlib-26208 | yes, 1m20s, 10, 111,809, $0.06 |
| pytest-dev__pytest-6197 | yes, 1m29s, 12, 126,726, $0.07 |
| django__django-12965 | yes, 0m50s, 6, 42,309, $0.04 |
| django__django-11999 | yes, 1m01s, 6, 32,903, $0.04 |
| sympy__sympy-19495 | no, 1m36s, 10, 89,021, $0.06 |
| django__django-15368 | yes, 0m39s, 5, 25,090, $0.03 |
| django__django-14007 | yes, 1m11s, 7, 42,361, $0.04 |
| sphinx-doc__sphinx-9230 | yes, 0m49s, 5, 38,064, $0.03 |
| psf__requests-1142 | yes, 1m47s, 9, 62,717, $0.05 |
| django__django-14011 | yes, 1m14s, 7, 53,000, $0.04 |
| astropy__astropy-14182 | yes, 0m52s, 5, 31,907, $0.03 |
| sympy__sympy-12096 | yes, 1m41s, 12, 93,040, $0.07 |
| django__django-13964 | yes, 0m59s, 7, 43,087, $0.04 |
| django__django-11206 | yes, 0m52s, 5, 25,285, $0.03 |
| matplotlib__matplotlib-23476 | yes, 1m35s, 8, 58,636, $0.05 |
| scikit-learn__scikit-learn-13328 | yes, 0m34s, 4, 21,221, $0.03 |
| django__django-14155 | no, 0m50s, 6, 30,419, $0.03 |
| pylint-dev__pylint-4551 | no, 2m20s, 10, 100,479, $0.09 |
| django__django-11555 | yes, 2m18s, 9, 83,922, $0.09 |
| sympy__sympy-24213 | yes, 0m42s, 6, 31,885, $0.03 |
| sphinx-doc__sphinx-7748 | no, 2m01s, 8, 85,455, $0.08 |
| pydata__xarray-6721 | yes, 1m02s, 6, 43,589, $0.03 |
| django__django-14559 | yes, 0m56s, 5, 26,675, $0.04 |
| django__django-12858 | yes, 0m57s, 6, 32,357, $0.03 |
| django__django-14053 | yes, 1m32s, 9, 72,128, $0.05 |
| sympy__sympy-16597 | no, 1m30s, 10, 74,003, $0.07 |
| pytest-dev__pytest-5787 | yes, 1m39s, 8, 77,808, $0.08 |
| django__django-11728 | yes, 1m04s, 6, 30,413, $0.03 |
| matplotlib__matplotlib-23314 | yes, 1m26s, 11, 66,307, $0.05 |
| django__django-14351 | yes, 1m33s, 9, 104,516, $0.08 |
| sphinx-doc__sphinx-9673 | yes, 1m08s, 7, 45,855, $0.05 |
| scikit-learn__scikit-learn-26194 | yes, 0m54s, 5, 29,411, $0.03 |
| django__django-14999 | yes, 0m46s, 6, 29,190, $0.03 |
| sympy__sympy-21847 | yes, 0m38s, 4, 21,579, $0.02 |
| django__django-16263 | no, 3m13s, 11, 130,987, $0.10 |
| astropy__astropy-7606 | yes, 0m52s, 7, 35,626, $0.03 |
| django__django-14122 | yes, 0m51s, 6, 36,555, $0.03 |

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
| sympy__sympy-21612 | 11 | 0 | bash 11, edit 4 | 0 | 0 | 4 / 0 / 0 |
| django__django-11333 | 6 | 0 | bash 5, edit 3, rlm.job 1, job.result 1 | 0 | 0 | 3 / 0 / 0 |
| scikit-learn__scikit-learn-13779 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| matplotlib__matplotlib-26208 | 9 | 0 | bash 9, edit 2, rlm.job 2 | 0 | 0 | 2 / 0 / 0 |
| pytest-dev__pytest-6197 | 11 | 0 | bash 12, edit 2, rlm.job 2, job.result 1, job2.result 1 | 0 | 0 | 2 / 0 / 0 |
| django__django-12965 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-11999 | 5 | 0 | bash 4, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-19495 | 9 | 0 | bash 8, edit 2, rlm.job 1, tests.job.result 1, checks.job.result 1 | 0 | 0 | 2 / 0 / 0 |
| django__django-15368 | 4 | 0 | bash 4, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-14007 | 6 | 0 | bash 6, edit 2, rlm.job 1, job.result 1 | 0 | 0 | 2 / 0 / 0 |
| sphinx-doc__sphinx-9230 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| psf__requests-1142 | 8 | 0 | bash 8, edit 3 | 0 | 0 | 3 / 0 / 0 |
| django__django-14011 | 6 | 0 | bash 6, edit 3 | 0 | 0 | 3 / 0 / 0 |
| astropy__astropy-14182 | 4 | 0 | bash 4, edit 3 | 0 | 0 | 3 / 0 / 0 |
| sympy__sympy-12096 | 11 | 0 | bash 11, edit 2, rlm.job 1, job.result 1 | 0 | 0 | 2 / 0 / 0 |
| django__django-13964 | 6 | 0 | bash 6, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-11206 | 4 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| matplotlib__matplotlib-23476 | 7 | 0 | bash 7, edit 3 | 0 | 0 | 3 / 0 / 0 |
| scikit-learn__scikit-learn-13328 | 3 | 0 | bash 4, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14155 | 5 | 0 | bash 4, edit 1, background.get 1, rlm.job 1 | 0 | 0 | 1 / 0 / 0 |
| pylint-dev__pylint-4551 | 9 | 1 | bash 9, read 1, edit 6 | 0 | 0 | 6 / 0 / 0 |
| django__django-11555 | 8 | 0 | bash 8, edit 5 | 0 | 0 | 5 / 0 / 0 |
| sympy__sympy-24213 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| sphinx-doc__sphinx-7748 | 7 | 0 | bash 7, read 1, edit 5 | 0 | 0 | 5 / 0 / 0 |
| pydata__xarray-6721 | 5 | 0 | bash 6, edit 2 | 0 | 0 | 2 / 0 / 0 |
| django__django-14559 | 4 | 0 | bash 4, edit 7 | 0 | 0 | 7 / 0 / 0 |
| django__django-12858 | 5 | 0 | bash 5, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14053 | 8 | 0 | bash 8, edit 3 | 0 | 0 | 3 / 0 / 0 |
| sympy__sympy-16597 | 8 | 0 | bash 9, edit 2, rlm.job 2, job.result 2 | 0 | 0 | 2 / 0 / 0 |
| pytest-dev__pytest-5787 | 7 | 0 | bash 7, read 1, write 1, edit 1 | 0 | 0 | 2 / 0 / 0 |
| django__django-11728 | 5 | 0 | bash 5, edit 6 | 0 | 0 | 6 / 0 / 0 |
| matplotlib__matplotlib-23314 | 10 | 1 | bash 7, edit 1, background.get 1, rlm.job 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14351 | 8 | 1 | bash 8, edit 7, read 1 | 0 | 0 | 5 / 0 / 0 |
| sphinx-doc__sphinx-9673 | 6 | 0 | bash 6, edit 1, background.get 1 | 0 | 0 | 1 / 0 / 0 |
| scikit-learn__scikit-learn-26194 | 4 | 0 | bash 5, edit 4, rlm.job 1, job.result 1 | 0 | 0 | 4 / 0 / 0 |
| django__django-14999 | 5 | 0 | bash 4, edit 1, rlm.job 1, job.result 1 | 0 | 0 | 1 / 0 / 0 |
| sympy__sympy-21847 | 3 | 0 | bash 4, read 1, write 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-16263 | 10 | 0 | bash 10, edit 12, rlm.job 4 | 0 | 0 | 12 / 0 / 0 |
| astropy__astropy-7606 | 6 | 0 | bash 6, edit 1 | 0 | 0 | 1 / 0 / 0 |
| django__django-14122 | 5 | 0 | bash 5, edit 2, rlm.job 1, job.result 1 | 0 | 0 | 2 / 0 / 0 |
