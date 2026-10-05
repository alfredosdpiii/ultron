---
name: interrogate
description: "Adversarial multi-model code review: run Ultron's /review once per model family plus a code-quality lens, group the findings by consensus, and sort them into Act on / Consider / Noted / Dismissed. Use for \"interrogate\", \"adversarial review\", \"multi-model review\", \"challenge this\", \"stress test this code\", \"find blind spots\", or \"tear this apart\"."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/interrogate
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Interrogate

Review the same changes once per model family. Every family gets the same review and the same lens; the adversarial
signal comes from model diversity, not assigned personas. The deliverable is a synthesized verdict. Nothing is
applied automatically.

Paths below are relative to this skill's directory (`SKILL_DIR`, as Ultron listed it).

## Step 1: Scope

Pick what to review, in `/review`'s target syntax:

- files or a diff the user pointed at: those paths;
- a feature branch: nothing (`/review` takes the branch since its merge base with the default branch plus uncommitted
  changes) or the base ref;
- a pull request: its number.

```python
SKILL_DIR = "..."  # this skill's directory
scope = ""  # e.g. "main", "123", "src/auth"
diff = await bash("git diff main...HEAD")  # the same changes as text, for the lens frames
```

## Step 2: State the intent

One clear paragraph from the user's message, commit messages (`git log main..HEAD`), the PR description
(`gh pr view <n>`) and the code. If you are unsure of the intent, ask in a plain reply and end the turn.

## Step 3: Review once per family

Pick the panel: the models the user names, else one per family.

```python
panel = []
for q in ["opus", "gpt", "gemini"]:  # example family queries
    ms = await rlm.find_models(q, limit=1)
    if ms:
        panel.append((q, f"{ms[0]['provider']}/{ms[0]['id']}"))
print(panel)  # say which families were missing
```

Two passes per family, run together:

1. `/review` on that model: its finders (bugs, security, architecture, tests, AI integration) and its verifier frames,
   which drop rejected findings and keep `confirmed` and `uncertain` ones.
2. The code-quality lens: one frame with `references/reviewer-prompt.md` filled with the intent, the diff,
   `references/rubric.md` and `references/code-quality-review.md`. `/review` does not see the intent or this lens.

```python
import review_api

template = await read(f"{SKILL_DIR}/references/reviewer-prompt.md")
rubric = await read(f"{SKILL_DIR}/references/rubric.md")
lens = await read(f"{SKILL_DIR}/references/code-quality-review.md")
INTENT = "..."
prompt = (template.replace("{INTENT}", INTENT).replace("{RUBRIC_CONTENTS}", rubric)
          .replace("{CODE_QUALITY_CONTENTS}", lens).replace("{DIFF_OR_FILES}", "(the diff is in the context)"))
FINDINGS = {"type": "array", "items": {"type": "object", "properties": {
    "file": {"type": "string"}, "line": {"type": "integer"},
    "severity": {"enum": ["blocker", "major", "minor", "nit"]}, "claim": {"type": "string"},
    "why": {"type": "string"}, "suggested_fix": {"type": "string"}},
    "required": ["file", "line", "severity", "claim", "why"]}}

async def lens_pass(model):
    try:
        return await rlm.infer(prompt, context=[diff.output], contract=FINDINGS, model=model)
    except Exception as error:  # InferenceError: report it as a gap, keep the other passes
        print("lens failed on", model, error)
        return None

async def one_family(label, model):
    review, quality = await asyncio.gather(
        review_api.run(rlm, f"--model {model} {scope}".strip()), lens_pass(model))
    found = [dict(f, status="confirmed") for f in review.confirmed]
    found += [dict(f, status="uncertain") for f in review.uncertain]
    found += [dict(f, status="lens", category="quality", confidence=0.5) for f in (quality or [])]
    return {"family": label, "model": model, "review": review, "findings": found, "lens_ok": bool(quality)}

runs = await asyncio.gather(*[one_family(label, model) for label, model in panel])
state["interrogate"] = {r["family"]: r["review"].report for r in runs}
for r in runs:
    print(r["family"], r["model"], repr(r["review"]), "lens:", "ok" if r["lens_ok"] else "incomplete")
```

A diff over roughly 60 KB is too big for one lens frame: run the lens on the files that matter most, or per file with
`rlm.map`. A run whose report says parts were not checked (budget, skipped files) is a coverage gap: list it.

## Step 4: Synthesize

Group the findings across families in code. `review_api.dedupe` merges findings on the same file within a few lines
that share a category or make the same claim, and unions their `reviewers`; tag each finding with its family first, so
`reviewers` becomes the list of families that raised it.

```python
pool = []
for r in runs:
    for f in r["findings"]:
        pool.append(dict(f, reviewers=[r["family"]], category=f.get("category", "quality")))
groups = review_api.dedupe(pool)
for g in groups:
    g["consensus"] = len(g["reviewers"])
consensus = [g for g in groups if g["consensus"] >= 2]
lone = [g for g in groups if g["consensus"] == 1]
print(len(groups), "findings,", len(consensus), "raised by 2+ families")
```

Consensus findings are the highest signal. Lone-family findings are still worth reading; weight them accordingly.
Look for disagreements too: one family flagging what another explicitly calls fine.

## Step 5: Lead judgment

You are the lead reviewer, a pragmatic senior engineer, not a neutral aggregator. Read
`references/lead-judgment.md`. One judge frame on a family other than yours drafts the buckets; you then check its
draft against what only you know (the conversation, the call sites, the plan) and move findings where it is wrong.

```python
MY_FAMILY = "opus"  # the family you run on
framework = await read(f"{SKILL_DIR}/references/lead-judgment.md")
items = [{k: g.get(k) for k in ("id", "file", "line", "severity", "claim", "why", "status", "reviewers")}
         for g in groups]
draft = await rlm.infer(
    "Categorize every finding: act_on (real correctness, security or maintainability issues that would block a "
    "real PR), consider (legitimate, unsure it is worth the cost now), noted (valid, not actionable), dismissed "
    "(wrong, nitpicky or missing context). One-line rationale each. Act on rarely needs more than 5 items.",
    context=[framework, f"Intent: {INTENT}", repr(items)],
    contract={"type": "array", "items": {"type": "object", "properties": {
        "id": {"type": "integer"}, "bucket": {"enum": ["act_on", "consider", "noted", "dismissed"]},
        "rationale": {"type": "string"}}, "required": ["id", "bucket", "rationale"]}},
    model=next((m for q, m in panel if q != MY_FAMILY), None),
)
```

Before moving a finding into Act on or out of it, trace it: read the cited lines and the callers
(`await bash(f"rg -n '{name}'")`). "What if this is null" is a finding only if a caller can pass null. Be slow to
dismiss security and correctness findings, even from one family. If the frame came back `Incomplete`, categorize the
rest yourself.

## Output

### Intent
> the paragraph from Step 2

### Reviewers
- one bullet per family: model, confirmed / uncertain / lens finding counts, coverage gaps

### Act on
Each: description, which families raised it, why it matters.

### Consider
Each: description, which families raised it, the tradeoff.

### Noted
Brief list.

### Dismissed
Each with a one-line reason.

### Agreement map
Where the families agreed, where they diverged, and what the pattern says.

Offer fixes; apply none until the user asks.
