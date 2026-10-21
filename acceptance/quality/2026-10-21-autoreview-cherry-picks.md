# Autoreview cherry-picks and promotions: what is left to read (redacted)

Date: 2026-10-21. Three private pull requests that carry merged pull requests to staging or release branches. All
three were reviewed line by line before, and the live reviews took 1.2, 4.2 to 5.7 and 7.9 hours. This measures
`review::promotion::promotion_delta` on their real diffs. Each diff runs from the merge base with the base commit
GitHub records to the head. The sources are the commits named by `git cherry-pick -x` trailers, plus the merged
pull requests of other base branches that the title or description names. No model calls.

| PR | How it names its sources | Changed lines | Read after | Files read | Sources kept | Files listed as left out |
|---|---|---|---|---|---|---|
| C | four `-x` picks, then 26 adaptation commits | 45,748 | 5,918 | 92 of 333 | 4 | 3 (5 lines) |
| D | one squashed commit; the description names its twin | 118,642 | 156 | 7 of 500 | 2 | 2 (4 lines) |
| E | ten `-x` picks of picks, no other commits | 18,314 | 0 | 0 of 59 | 11 | 8 (92 lines) |

Findings:

- What is left is the work the authors did on the target branch: C's adaptation commits, D's prompt receipt, and
  E's nothing.
- E's listed lines are real. E brings its restore fix over through the staging branch's adapted commits, so the
  development branch's version of those lines is not in its head.
- A first version listed every source-added line that is not in the head: 405 lines in C and 1,932 in E. Most of
  those were lines that a later source in a chain replaced, or that an adaptation rewrote in a file the review reads
  anyway. Lines that a source removes, and files still in the reviewed diff, are no longer listed.
