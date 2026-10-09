# Autoreview benchmark: opencode/claude-haiku-5-5@high/high, cliproxyapi/gpt-6-luna@high/high

2026-10-09. 40 cases (20 buggy, 20 clean) from SWE-bench/SWE-bench_Verified, seed `ultron-autoreview-1`, 1 trial per case.
Reviewer: `~/.local/share/mise/installs/node/26.8.1/bin/node ~/Projects/personal/ultron-wt-armerge/packages/coding-agent/dist/cli.js autoreview review --repo-dir <repo> --base <base> --head <head> --model <model> --json --dry-run` (ultron 0.87.35).
Judge: `cliproxyapi/glm-5.3-flash`, one call per confirmed finding of minor severity or worse on buggy cases.

## Summary

| arm | recall (blocker/major) | recall (+minor) | recall (changed lines) | recall (judged) | false alarms | verdict accuracy | findings / review | p50 s | p90 s | max s | tokens in / out per review | cost per review | errors |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| opencode/claude-haiku-5-5@high/high | 6/20 (30%) | 12/20 (60%) | 6/20 (30%) | 25% | 2/20 (10%) | 70% | 2.4 | 90.5 | 181.8 | 480.1 | 60 / 46,738 | $0.039 | 0 |
| cliproxyapi/gpt-6-luna@high/high | 6/20 (30%) | 13/20 (65%) | 6/20 (30%) | 25% | 4/20 (20%) | 60% | 1.6 | 146.8 | 322.7 | 558.9 | 97,183 / 10,651 | $0.015 | 0 |

Recall is over buggy cases: a confirmed finding inside a ground-truth hunk (+-5 lines) at blocker or major severity; `+minor` also
accepts minor; `changed lines` requires the finding within 2 lines of a line the diff changed; `judged` is a judge model's verdict that
a confirmed blocker or major finding describes the reported defect. False alarms are clean cases with a confirmed blocker or major finding.

## Detail per arm

| arm | buggy: request_changes | clean: approve | clean findings (blocker/major/minor/nit) | buggy findings (blocker/major/minor/nit) | reviewer ms (scope/find/verify) | frames / review | tokens in / out total | cost total | incomplete |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| opencode/claude-haiku-5-5@high/high | 10/20 (50%) | 18/20 (90%) | 0/3/48/0 | 0/7/36/0 | 38/16,218/9,414 | 15.1 | 2,412 / 1,869,534 | $1.56 | 1 |
| cliproxyapi/gpt-6-luna@high/high | 11/20 (55%) | 13/20 (65%) | 0/5/29/0 | 0/8/20/0 | 45/61,181/14,896 | 15.3 | 3,887,323 / 426,022 | $0.610 | 0 |

## Cases

| case | repository | files | hunks | changed lines | extra hunks |
| --- | --- | --- | --- | --- | --- |
| buggy-sphinx-doc__sphinx-8120 | sphinx-doc/sphinx | 2 | 3 | 9 | 2 |
| clean-pallets__flask-5014 | pallets/flask | 1 | 1 | 3 | 0 |
| buggy-astropy__astropy-7336 | astropy/astropy | 1 | 1 | 2 | 2 |
| clean-mwaskom__seaborn-3187 | mwaskom/seaborn | 2 | 2 | 12 | 0 |
| buggy-pydata__xarray-4075 | pydata/xarray | 1 | 1 | 9 | 2 |
| clean-sympy__sympy-16450 | sympy/sympy | 1 | 1 | 2 | 0 |
| buggy-pylint-dev__pylint-7277 | pylint-dev/pylint | 1 | 1 | 5 | 2 |
| clean-pytest-dev__pytest-10081 | pytest-dev/pytest | 1 | 1 | 5 | 0 |
| buggy-scikit-learn__scikit-learn-11310 | scikit-learn/scikit-learn | 1 | 4 | 14 | 2 |
| clean-django__django-15563 | django/django | 2 | 3 | 25 | 0 |
| buggy-matplotlib__matplotlib-24149 | matplotlib/matplotlib | 1 | 1 | 8 | 2 |
| clean-psf__requests-1921 | psf/requests | 1 | 1 | 2 | 0 |
| buggy-sphinx-doc__sphinx-7889 | sphinx-doc/sphinx | 1 | 1 | 4 | 2 |
| clean-astropy__astropy-13033 | astropy/astropy | 1 | 2 | 14 | 0 |
| buggy-mwaskom__seaborn-3069 | mwaskom/seaborn | 1 | 4 | 15 | 2 |
| clean-pydata__xarray-6721 | pydata/xarray | 1 | 1 | 2 | 0 |
| buggy-sympy__sympy-18698 | sympy/sympy | 1 | 3 | 11 | 2 |
| clean-pylint-dev__pylint-4551 | pylint-dev/pylint | 4 | 7 | 126 | 0 |
| buggy-pytest-dev__pytest-5262 | pytest-dev/pytest | 1 | 1 | 4 | 2 |
| clean-scikit-learn__scikit-learn-26323 | scikit-learn/scikit-learn | 1 | 2 | 4 | 0 |
| buggy-django__django-10999 | django/django | 1 | 1 | 7 | 2 |
| clean-matplotlib__matplotlib-26291 | matplotlib/matplotlib | 1 | 1 | 2 | 0 |
| buggy-psf__requests-1766 | psf/requests | 1 | 2 | 4 | 2 |
| clean-sphinx-doc__sphinx-10323 | sphinx-doc/sphinx | 1 | 1 | 4 | 0 |
| buggy-astropy__astropy-12907 | astropy/astropy | 1 | 1 | 2 | 2 |
| clean-pydata__xarray-3993 | pydata/xarray | 2 | 5 | 49 | 0 |
| buggy-sympy__sympy-20801 | sympy/sympy | 1 | 2 | 4 | 2 |
| clean-pylint-dev__pylint-4604 | pylint-dev/pylint | 2 | 3 | 6 | 0 |
| buggy-pytest-dev__pytest-8399 | pytest-dev/pytest | 2 | 5 | 10 | 2 |
| clean-scikit-learn__scikit-learn-14087 | scikit-learn/scikit-learn | 1 | 2 | 9 | 0 |
| buggy-django__django-14007 | django/django | 1 | 2 | 21 | 2 |
| clean-matplotlib__matplotlib-25479 | matplotlib/matplotlib | 2 | 2 | 7 | 0 |
| buggy-psf__requests-6028 | psf/requests | 1 | 1 | 4 | 2 |
| clean-sphinx-doc__sphinx-8551 | sphinx-doc/sphinx | 2 | 2 | 3 | 0 |
| buggy-astropy__astropy-7166 | astropy/astropy | 1 | 3 | 10 | 2 |
| clean-pydata__xarray-7229 | pydata/xarray | 1 | 2 | 29 | 0 |
| buggy-sympy__sympy-16766 | sympy/sympy | 1 | 1 | 5 | 2 |
| clean-pylint-dev__pylint-4970 | pylint-dev/pylint | 1 | 1 | 2 | 0 |
| buggy-pytest-dev__pytest-7432 | pytest-dev/pytest | 1 | 1 | 3 | 2 |
| clean-scikit-learn__scikit-learn-15100 | scikit-learn/scikit-learn | 1 | 1 | 9 | 0 |

## Runs

| arm | case | trial | status | verdict | caught | +minor | changed lines | judged | false alarm | findings (b/M/m/n) | seconds | tokens in/out | cost |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| opencode/claude-haiku-5-5@high/high | buggy-sphinx-doc__sphinx-8120 | 1 | ok | request_changes | no | yes | no | no | - | 0/0/8/0 | 175.6 | 96/102,706 | $0.080 |
| opencode/claude-haiku-5-5@high/high | clean-pallets__flask-5014 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 61.5 | 48/19,043 | $0.018 |
| opencode/claude-haiku-5-5@high/high | buggy-astropy__astropy-7336 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/1/0 | 79.7 | 48/28,593 | $0.026 |
| opencode/claude-haiku-5-5@high/high | clean-mwaskom__seaborn-3187 | 1 | ok | approve | - | - | - | - | no | 0/0/2/0 | 77.0 | 52/23,797 | $0.025 |
| opencode/claude-haiku-5-5@high/high | buggy-pydata__xarray-4075 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/1/0 | 84.5 | 40/27,964 | $0.025 |
| opencode/claude-haiku-5-5@high/high | clean-sympy__sympy-16450 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 87.4 | 48/30,164 | $0.025 |
| opencode/claude-haiku-5-5@high/high | buggy-pylint-dev__pylint-7277 | 1 | ok | request_changes | no | yes | no | no | - | 0/0/2/0 | 120.8 | 84/55,932 | $0.048 |
| opencode/claude-haiku-5-5@high/high | clean-pytest-dev__pytest-10081 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 71.1 | 72/31,718 | $0.032 |
| opencode/claude-haiku-5-5@high/high | buggy-scikit-learn__scikit-learn-11310 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/0/0 | 34.4 | 32/6,419 | $0.010 |
| opencode/claude-haiku-5-5@high/high | clean-django__django-15563 | 1 | ok | approve | - | - | - | - | no | 0/0/3/0 | 181.8 | 68/72,087 | $0.056 |
| opencode/claude-haiku-5-5@high/high | buggy-matplotlib__matplotlib-24149 | 1 | ok | request_changes | no | no | no | no | - | 0/0/1/0 | 76.5 | 48/16,850 | $0.020 |
| opencode/claude-haiku-5-5@high/high | clean-psf__requests-1921 | 1 | ok | approve | - | - | - | - | no | 0/1/1/0 | 85.4 | 48/29,223 | $0.022 |
| opencode/claude-haiku-5-5@high/high | buggy-sphinx-doc__sphinx-7889 | 1 | ok | approve (wrong) | no | yes | no | no | - | 0/1/1/0 | 123.8 | 64/56,243 | $0.044 |
| opencode/claude-haiku-5-5@high/high | clean-astropy__astropy-13033 | 1 | ok | approve | - | - | - | - | no | 0/0/3/0 | 101.6 | 88/54,147 | $0.045 |
| opencode/claude-haiku-5-5@high/high | buggy-mwaskom__seaborn-3069 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/4/0 | 124.8 | 68/58,143 | $0.051 |
| opencode/claude-haiku-5-5@high/high | clean-pydata__xarray-6721 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 80.9 | 52/23,939 | $0.020 |
| opencode/claude-haiku-5-5@high/high | buggy-sympy__sympy-18698 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/2/0 | 201.5 | 52/56,922 | $0.046 |
| opencode/claude-haiku-5-5@high/high | clean-pylint-dev__pylint-4551 | 1 | ok | request_changes (wrong) | - | - | - | - | yes | 0/1/8/0 | 480.1 | 100/228,350 | $0.157 |
| opencode/claude-haiku-5-5@high/high | buggy-pytest-dev__pytest-5262 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/0/0 | 22.2 | 28/4,520 | $0.007 |
| opencode/claude-haiku-5-5@high/high | clean-scikit-learn__scikit-learn-26323 | 1 | ok | approve | - | - | - | - | no | 0/0/2/0 | 100.5 | 52/32,712 | $0.029 |
| opencode/claude-haiku-5-5@high/high | buggy-django__django-10999 | 1 | ok | request_changes | yes | yes | yes | no | - | 0/1/1/0 | 163.6 | 64/73,666 | $0.050 |
| opencode/claude-haiku-5-5@high/high | clean-matplotlib__matplotlib-26291 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 66.7 | 48/20,726 | $0.021 |
| opencode/claude-haiku-5-5@high/high | buggy-psf__requests-1766 | 1 | ok | approve (wrong) | no | yes | no | no | - | 0/0/1/0 | 133.3 | 84/75,392 | $0.059 |
| opencode/claude-haiku-5-5@high/high | clean-sphinx-doc__sphinx-10323 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 80.4 | 52/24,907 | $0.025 |
| opencode/claude-haiku-5-5@high/high | buggy-astropy__astropy-12907 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/1/0 | 105.6 | 48/37,639 | $0.031 |
| opencode/claude-haiku-5-5@high/high | clean-pydata__xarray-3993 | 1 | ok | approve | - | - | - | - | no | 0/0/7/0 | 121.9 | 92/85,179 | $0.076 |
| opencode/claude-haiku-5-5@high/high | buggy-sympy__sympy-20801 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/2/0 | 214.8 | 52/72,971 | $0.053 |
| opencode/claude-haiku-5-5@high/high | clean-pylint-dev__pylint-4604 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 90.5 | 80/41,386 | $0.037 |
| opencode/claude-haiku-5-5@high/high | buggy-pytest-dev__pytest-8399 | 1 | ok | approve (wrong) | no | yes | no | no | - | 0/0/3/0 | 114.2 | 76/59,627 | $0.063 |
| opencode/claude-haiku-5-5@high/high | clean-scikit-learn__scikit-learn-14087 | 1 | ok | approve | - | - | - | - | no | 0/0/2/0 | 83.3 | 72/40,944 | $0.039 |
| opencode/claude-haiku-5-5@high/high | buggy-django__django-14007 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/2/0 | 126.7 | 52/47,823 | $0.040 |
| opencode/claude-haiku-5-5@high/high | clean-matplotlib__matplotlib-25479 | 1 | ok | approve | - | - | - | - | no | 0/0/4/0 | 106.2 | 68/48,179 | $0.043 |
| opencode/claude-haiku-5-5@high/high | buggy-psf__requests-6028 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/0/0 | 27.0 | 40/5,366 | $0.009 |
| opencode/claude-haiku-5-5@high/high | clean-sphinx-doc__sphinx-8551 | 1 | ok | approve | - | - | - | - | no | 0/0/4/0 | 78.5 | 60/27,716 | $0.028 |
| opencode/claude-haiku-5-5@high/high | buggy-astropy__astropy-7166 | 1 | ok | request_changes | no | yes | no | no | - | 0/0/3/0 | 104.5 | 52/37,873 | $0.035 |
| opencode/claude-haiku-5-5@high/high | clean-pydata__xarray-7229 | 1 | ok | request_changes (wrong) | - | - | - | - | yes | 0/1/2/0 | 206.1 | 64/94,566 | $0.063 |
| opencode/claude-haiku-5-5@high/high | buggy-sympy__sympy-16766 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/2/0 | 50.9 | 52/17,998 | $0.022 |
| opencode/claude-haiku-5-5@high/high | clean-pylint-dev__pylint-4970 | 1 | ok | approve | - | - | - | - | no | 0/0/2/0 | 70.0 | 68/27,348 | $0.026 |
| opencode/claude-haiku-5-5@high/high | buggy-pytest-dev__pytest-7432 | 1 | ok (incomplete) | comment (wrong) | no | no | no | no | - | 0/0/1/0 | 136.9 | 56/50,882 | $0.039 |
| opencode/claude-haiku-5-5@high/high | clean-scikit-learn__scikit-learn-15100 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 67.6 | 44/19,874 | $0.019 |
| cliproxyapi/gpt-6-luna@high/high | buggy-sphinx-doc__sphinx-8120 | 1 | ok | request_changes | no | yes | no | no | - | 0/1/2/0 | 322.7 | 179,766/15,763 | $0.026 |
| cliproxyapi/gpt-6-luna@high/high | clean-pallets__flask-5014 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 76.6 | 54,024/4,952 | $0.008 |
| cliproxyapi/gpt-6-luna@high/high | buggy-astropy__astropy-7336 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/1/0 | 111.5 | 76,385/7,070 | $0.011 |
| cliproxyapi/gpt-6-luna@high/high | clean-mwaskom__seaborn-3187 | 1 | ok | request_changes (wrong) | - | - | - | - | no | 0/0/2/0 | 126.4 | 63,198/8,159 | $0.011 |
| cliproxyapi/gpt-6-luna@high/high | buggy-pydata__xarray-4075 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/1/0 | 141.6 | 68,133/7,456 | $0.011 |
| cliproxyapi/gpt-6-luna@high/high | clean-sympy__sympy-16450 | 1 | ok | request_changes (wrong) | - | - | - | - | yes | 0/1/1/0 | 115.5 | 53,330/8,278 | $0.010 |
| cliproxyapi/gpt-6-luna@high/high | buggy-pylint-dev__pylint-7277 | 1 | ok | request_changes | no | yes | no | no | - | 0/0/2/0 | 158.7 | 126,729/11,979 | $0.019 |
| cliproxyapi/gpt-6-luna@high/high | clean-pytest-dev__pytest-10081 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 242.4 | 105,708/7,851 | $0.015 |
| cliproxyapi/gpt-6-luna@high/high | buggy-scikit-learn__scikit-learn-11310 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/0/0 | 60.4 | 52,582/3,360 | $0.007 |
| cliproxyapi/gpt-6-luna@high/high | clean-django__django-15563 | 1 | ok | approve | - | - | - | - | no | 0/0/2/0 | 137.5 | 100,112/12,839 | $0.017 |
| cliproxyapi/gpt-6-luna@high/high | buggy-matplotlib__matplotlib-24149 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/0/0 | 126.2 | 68,032/4,278 | $0.009 |
| cliproxyapi/gpt-6-luna@high/high | clean-psf__requests-1921 | 1 | ok | request_changes (wrong) | - | - | - | - | no | 0/0/2/0 | 288.2 | 48,712/13,460 | $0.012 |
| cliproxyapi/gpt-6-luna@high/high | buggy-sphinx-doc__sphinx-7889 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/1/0 | 188.0 | 113,421/12,313 | $0.018 |
| cliproxyapi/gpt-6-luna@high/high | clean-astropy__astropy-13033 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 162.9 | 106,882/12,084 | $0.017 |
| cliproxyapi/gpt-6-luna@high/high | buggy-mwaskom__seaborn-3069 | 1 | ok | request_changes | no | yes | no | no | - | 0/0/2/0 | 284.7 | 157,042/16,258 | $0.024 |
| cliproxyapi/gpt-6-luna@high/high | clean-pydata__xarray-6721 | 1 | ok | approve | - | - | - | - | no | 0/0/2/0 | 391.2 | 64,823/17,756 | $0.016 |
| cliproxyapi/gpt-6-luna@high/high | buggy-sympy__sympy-18698 | 1 | ok | approve (wrong) | no | yes | no | no | - | 0/0/2/0 | 558.9 | 100,542/22,749 | $0.022 |
| cliproxyapi/gpt-6-luna@high/high | clean-pylint-dev__pylint-4551 | 1 | ok | request_changes (wrong) | - | - | - | - | yes | 0/2/5/0 | 499.0 | 191,951/39,902 | $0.040 |
| cliproxyapi/gpt-6-luna@high/high | buggy-pytest-dev__pytest-5262 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/0/0 | 63.6 | 50,772/2,195 | $0.006 |
| cliproxyapi/gpt-6-luna@high/high | clean-scikit-learn__scikit-learn-26323 | 1 | ok | request_changes (wrong) | - | - | - | - | yes | 0/1/1/0 | 286.7 | 80,288/10,568 | $0.014 |
| cliproxyapi/gpt-6-luna@high/high | buggy-django__django-10999 | 1 | ok | request_changes | yes | yes | yes | no | - | 0/2/1/0 | 300.5 | 92,196/20,188 | $0.019 |
| cliproxyapi/gpt-6-luna@high/high | clean-matplotlib__matplotlib-26291 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 215.5 | 71,635/7,157 | $0.011 |
| cliproxyapi/gpt-6-luna@high/high | buggy-psf__requests-1766 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/1/0 | 104.9 | 142,992/11,061 | $0.020 |
| cliproxyapi/gpt-6-luna@high/high | clean-sphinx-doc__sphinx-10323 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 158.2 | 87,516/5,728 | $0.012 |
| cliproxyapi/gpt-6-luna@high/high | buggy-astropy__astropy-12907 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/0/0 | 206.9 | 79,114/8,539 | $0.012 |
| cliproxyapi/gpt-6-luna@high/high | clean-pydata__xarray-3993 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 348.1 | 135,563/10,993 | $0.020 |
| cliproxyapi/gpt-6-luna@high/high | buggy-sympy__sympy-20801 | 1 | ok | request_changes | yes | yes | yes | yes | - | 0/1/1/0 | 146.6 | 95,593/10,184 | $0.015 |
| cliproxyapi/gpt-6-luna@high/high | clean-pylint-dev__pylint-4604 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 215.8 | 122,896/9,878 | $0.017 |
| cliproxyapi/gpt-6-luna@high/high | buggy-pytest-dev__pytest-8399 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/0/0 | 146.8 | 177,829/10,931 | $0.024 |
| cliproxyapi/gpt-6-luna@high/high | clean-scikit-learn__scikit-learn-14087 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 103.0 | 131,288/9,228 | $0.018 |
| cliproxyapi/gpt-6-luna@high/high | buggy-django__django-14007 | 1 | ok | request_changes | no | yes | no | no | - | 0/0/2/0 | 171.8 | 104,171/8,240 | $0.015 |
| cliproxyapi/gpt-6-luna@high/high | clean-matplotlib__matplotlib-25479 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 190.0 | 131,314/10,027 | $0.018 |
| cliproxyapi/gpt-6-luna@high/high | buggy-psf__requests-6028 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/0/0 | 46.2 | 44,186/2,751 | $0.006 |
| cliproxyapi/gpt-6-luna@high/high | clean-sphinx-doc__sphinx-8551 | 1 | ok | request_changes (wrong) | - | - | - | - | no | 0/0/2/0 | 134.2 | 75,370/9,492 | $0.012 |
| cliproxyapi/gpt-6-luna@high/high | buggy-astropy__astropy-7166 | 1 | ok | request_changes | no | yes | no | no | - | 0/0/2/0 | 136.0 | 94,866/7,312 | $0.013 |
| cliproxyapi/gpt-6-luna@high/high | clean-pydata__xarray-7229 | 1 | ok | request_changes (wrong) | - | - | - | - | yes | 0/1/1/0 | 142.8 | 107,329/9,782 | $0.016 |
| cliproxyapi/gpt-6-luna@high/high | buggy-sympy__sympy-16766 | 1 | ok | approve (wrong) | no | no | no | no | - | 0/0/1/0 | 221.3 | 123,442/11,343 | $0.019 |
| cliproxyapi/gpt-6-luna@high/high | clean-pylint-dev__pylint-4970 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 135.7 | 76,183/7,347 | $0.011 |
| cliproxyapi/gpt-6-luna@high/high | buggy-pytest-dev__pytest-7432 | 1 | ok | approve (wrong) | no | yes | no | no | - | 0/0/1/0 | 127.2 | 65,364/9,469 | $0.012 |
| cliproxyapi/gpt-6-luna@high/high | clean-scikit-learn__scikit-learn-15100 | 1 | ok | approve | - | - | - | - | no | 0/0/1/0 | 106.1 | 66,044/7,102 | $0.010 |
