### Eval

**You own the experiment design. Plan, blind, run, synthesize.**

**Blinding, non-negotiable:**

- No `eval`, `test`, `judge`, `experiment`, `rubric`, `score`, `compare`, `benchmark`, `candidate` or `arena` in any
  directory, file, child name or brief the candidate sees.
- The candidate brief reads like an organic user request: the goal, not the meta.
- No chain-eliciting cues. Don't ask candidates to list the skills, principles or files they used. Ask for design notes
  generally and grade chain-following from what they did.
- Project-shaped directory and slug names a user might pick.
- Don't tell a candidate other candidates exist.
- The judge knows it is judging but sees outputs by sanitized label only, never by model name.
- Two variants: one judge scores both sets in one pass on one scale, blind to which set each came from.

**Steps:**

1. **Frame.** The variant under test and what behavior counts as success. Write a 3-6 criterion rubric for the judge
   only.
2. **Set up sanitized environments.** One working dir per candidate with the variant in place and the context an
   organic task would have (a project skeleton, the skills the candidate would naturally read).
3. **Author one organic prompt.** What a user would type, with no leakage of what is measured.
4. **Run N candidates in parallel on different models**, per `../arena/SKILL.md`: one `rlm.spawn` per candidate with
   the same prompt, each in its own sanitized dir, `model=` from different `rlm.find_models` results.
5. **Judge blind on a different model family**, per `../arena/SKILL.md`: one `rlm.infer` frame with the rubric and the
   labeled outputs, for example `contract={"scores": dict, "verdict": str, "reasons": str}`.
6. **Verify the chain from evidence, not self-report.** What each candidate changed (its worktree result's
   `changed_files` and diff) and the shape of its code; for which files it read, its session transcript (found and parsed
   with `../recall/scripts/sessions.py`; a child's own session file names its parent). Never read sessions of unrelated projects.
7. **Read every candidate output yourself** end to end and compare with the judge. Disagreement means a biased model or
   an ambiguous rubric. Synthesize.

**Reply:** the variant under test, the rubric, per-candidate notes, the judge's verdict, your synthesis, and whether to
promote the variant.
