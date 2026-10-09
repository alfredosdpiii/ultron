# Autoreview: structural reference, precision rule, reference view, learning loop (2026-10-09)

Three builds, two benchmarks, two arms. Numbers are on the cases every compared run had scored (interim: the
runs were stopped at the sample sizes below on purpose). Runs: `ar-base-1009` (0.87.35, the old code),
`ar-ref-1009` (reference rendered into every frame + tests-finding rule), `ar-ref2-1009` (the same with
`referenceView: investigators`, the shipped default, plus the learning loop). Judges: `cliproxyapi/glm-5.3-flash`
(SWE), `cliproxyapi/gpt-6.1-sol` at high thinking (blind). The `opencode-go` and `opencode` Haiku arms that were
started first are not reported: the first lost its quota mid-run (23 of 40 reviews failed), the second was
stopped on request; the Haiku arm below runs through Claude Code (`claude-code/claude-haiku-5-5@high/high`).

## SWE-bench Verified reversed fixes (buggy: the fix reverted; clean: the fix)

| arm | cases (buggy/clean) | caught bug | false alarms | verdict accuracy | findings/review | p50 s | $/review |
|---|---|---|---|---|---|---|---|
| luna, old | 29 (15/14) | 6/15 (40%) | 3/14 (21%) | 62% | 1.66 | 163 | 0.016 |
| luna, reference | 29 (15/14) | 5/15 (33%) | 3/14 (21%) | 55% | 1.38 | 141 | 0.014 |
| Haiku 5.5 (Claude Code), old | 19 (10/9) | 2/10 (20%) | 0/9 (0%) | 68% | 2.05 | 160 | 0.075 |
| Haiku 5.5 (Claude Code), reference | 19 (10/9) | 3/10 (30%) | 0/9 (0%) | 68% | 2.21 | 173 | 0.078 |

Full old-code run (40 cases, both arms): luna caught 6/20, 4/20 false alarms, 60% verdicts; Haiku caught 6/20,
2/20 false alarms, 70% verdicts (`2026-10-09-autoreview-bench-20x2-baseline-v0.87.35-*.md`).

## Blind set `ref1` (30 real pull requests, a human reviewer's comments as the reference; redacted)

| arm | PRs | recall (same issue) | with partial | his major points | confirmed findings matching him | precision | findings/review | invalid extras | verdict agreement | p50 s | $/review |
|---|---|---|---|---|---|---|---|---|---|---|---|
| luna, old | 12 | 18% (5/28) | 21% | 2/9 | 5 | 58% | 5.6 | 9 | 64% | 307 | 0.051 |
| luna, reference | 12 | 14% (4/28) | 21% | 1/9 | 6 | 65% | 3.4 | 3 | 55% | 199 | 0.045 |
| Haiku (Claude Code), old | 4 | 29% (2/7) | 57% | 0/1 | 5 | 48% | 7.5 | 8 | 33% | 209 | 0.218 |
| Haiku (Claude Code), reference in every frame | 4 | 29% | 43% | 0/1 | 2 | 41% | 6.8 | 4 | 0% | 231 | 0.235 |
| Haiku (Claude Code), reference as lookups (shipped) | 4 | 29% | 71% | 0/1 | 4 | 67% | 8.5 | 1 | 0% | 244 | 0.217 |

Earlier, before its quota ran out, the `opencode-go` Haiku arm on ~23 shared PRs: 34% recall (45% with partial)
on the old code against 23% (32%) with the reference in every frame.

## Reading

- The precision half holds on the recommended model: invalid extras 9 -> 3, findings per review -40%, precision
  +7 points, 35% faster. Nearly all of it is the tests-finding rule (no run proved it: a low, non-blocking note)
  and the configuration-literal rule.
- Recall is flat to slightly down on luna; verdicts fell about 7 points on both sets because a review whose only
  confirmed finding is a tests note no longer requests changes. That is the intended trade.
- Rendering the reference into every frame hurts the small model (Haiku: precision and partial recall down on
  both providers), which SWE-PRBench reports for every model it tested. With the rendering kept to the
  investigators (`referenceView: investigators`, the default), Haiku's precision rose from 48% to 67% and
  invalid extras fell from 8 to 1 on the same four PRs; the lookups, resolved callers and structural findings
  stay. Four PRs is a direction, not a measurement: rerun `ar-ref2-1009` to 30 before relying on it.
- Haiku 5.5 through Claude Code matches the human reviewer more often than luna (29-34% against 14-18%) at
  3-4x the posted findings and 4-5x the cost per review.
