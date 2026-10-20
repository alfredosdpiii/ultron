# Autoreview knowledge graph: offline replays (redacted)

Date: 2026-10-21. Two private pull requests (A: a new append-only table whose migration refuses UPDATE but not
TRUNCATE, which the live review approved and three human reviewers blocked; B: a background purge job where two
live reviews of the same commit named different blockers). Replayed with `ultron autoreview review --repo-dir
--base --head --json --mode both --no-run-tests`, model `cliproxyapi/gpt-6-astra` (the live reviews' model),
private agent and server directories, nothing posted. Offline replays have no pull request title, description,
lessons or other reviewers' comments, unlike the live reviews.

| Arm | PR | Verdict | Findings (level/verification) | Frames | Nominal cost |
|---|---|---|---|---|---|
| baseline (rust-rewrite) | A | approve | low/confirmed x2, low/uncertain x1 | 58 | $3.37 |
| knowledge, brief before the placement fix | A | approve | low/confirmed x2 | 57 | $4.62 |
| knowledge, brief before the placement fix | A | approve | low/confirmed x2 | 58 | $3.11 |
| knowledge, final brief | A | request changes | medium/confirmed x1, low/confirmed x2 | 58 | $3.57 |
| baseline (rust-rewrite) | B | approve | low/confirmed x1 | 58 | $3.44 |
| knowledge, brief before the placement fix | B | approve | low/confirmed x1 | 58 | $4.83 |
| knowledge, brief before the placement fix | B | approve | low/confirmed x1 | 58 | $2.77 |

Costs are list prices without cache reads; the runs went through a subscription proxy.

Findings:

- No arm found the defect of A (TRUNCATE not refused). Its migration's most similar file by rare constructs is the
  repository's other append-only audit migration, which refuses TRUNCATE and explains why; `similar()` ranks it
  first on the real repository.
- The first knowledge runs never showed that pointer: the brief's "Like" lines came after the long sections and a
  15-file change filled the 8,000-character brief before them. They now follow the claims (fixed before the last A
  run). One run with them is not enough to tell their effect from run-to-run variance.
- The offline setup, not the change, explains the gap to the live reviews: the baseline arm finds the same class of
  low findings on both pull requests.
