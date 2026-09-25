# Ultron A01-A46 acceptance report

Generated 2026-09-25T05:43:39.325Z by `npm run test:acceptance`. Unavailable is never passed; partial evidence is never a pass.

- Instrument lock: instrument lock matches
- Test runner: exit 0
- Rows: 42 passed, 0 failed, 4 unverified, 0 blocked (of 46)

| Row | Status | Evidence (passed/listed) | Reasons |
|---|---|---|---|
| A01 | passed | 3/3 | - |
| A02 | passed | 2/2 | - |
| A03 | passed | 7/7 | - |
| A04 | passed | 1/1 | - |
| A05 | passed | 15/15 | - |
| A06 | passed | 9/9 | - |
| A07 | passed | 4/4 | - |
| A08 | passed | 6/6 | - |
| A09 | passed | 7/7 | - |
| A10 | passed | 2/2 | - |
| A11 | passed | 9/9 | - |
| A12 | passed | 7/7 | - |
| A13 | passed | 3/3 | - |
| A14 | passed | 6/6 | - |
| A15 | passed | 9/9 | - |
| A16 | unverified | 2/2 | evidence green but incomplete for this row. Skip and scope pinning proven against the Hindsight HTTP adapter with a captured fake. Consolidation runs inside Hindsight and has not been exercised against a live backend. |
| A17 | passed | 2/2 | - |
| A18 | passed | 4/4 | - |
| A19 | passed | 3/3 | - |
| A20 | passed | 1/1 | - |
| A21 | passed | 2/2 | - |
| A22 | passed | 3/3 | - |
| A23 | passed | 10/10 | - |
| A24 | passed | 2/2 | - |
| A25 | passed | 13/13 | - |
| A26 | unverified | 0/0 | no evidence. mutation slice 10/10 killed. Evidence is the instrument lock plus the mutation slice (acceptance/mutation.json); a surviving mutation holds the row unverified. Independent human review of the instrument remains a process step. |
| A27 | passed | 3/3 | - |
| A28 | passed | 5/5 | - |
| A29 | passed | 2/2 | - |
| A30 | passed | 3/3 | - |
| A31 | passed | 10/10 | - |
| A32 | passed | 5/5 | - |
| A33 | passed | 3/3 | - |
| A34 | passed | 1/1 | - |
| A35 | passed | 5/5 | - |
| A36 | passed | 4/4 | - |
| A37 | passed | 7/7 | - |
| A38 | passed | 3/3 | - |
| A39 | unverified | 0/0 | no evidence. Metered live suite; runs only with ULTRON_LIVE_EVAL=1, so the default report shows it unverified. Recorded runs: acceptance/capabilities/*.json. |
| A40 | passed | 4/4 | - |
| A41 | passed | 13/13 | - |
| A42 | passed | 2/2 | - |
| A43 | passed | 7/7 | - |
| A44 | passed | 7/7 | - |
| A45 | passed | 2/2 | - |
| A46 | unverified | 2/2 | evidence green but incomplete for this row. The deterministic demonstration and failure variants pass with a scripted model. The plan also requires a live-model demonstration, which is recorded separately. |
