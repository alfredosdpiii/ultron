---
name: arena
description: "Run N parallel candidate implementations of the same task on different models, judge them against a rubric, pick a base and graft the strongest parts of the others into it. Use for /skill:arena, 'arena this', 'throw it in the arena', or when one attempt at a non-trivial artifact would lock in the wrong shape."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/arena
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Arena

Fan out N parallel attempts at the same task. Read every candidate end to end. Pick the strongest as the base. Graft
the best ideas from the others into it. Verify the synthesized result.

```python
state["plan"] = ["frame", "fan out", "cross-judge", "pick", "graft", "verify"]
```

## Phase A: Frame

The N candidates receive the same prompt, so the prompt is the contract.

1. State the artifact each candidate produces.
2. Derive the rubric. State what success looks like for *this* task, then turn it into 3-6 concrete gradeable
   criteria. The rubric is the picker's tool in Phase D. Candidates see only the task.
3. Pick the runners: by default one per model family, so the differences between candidates come from different
   models. Same model N times when the work is generation-bound rather than judgment-sensitive; more runners when the
   arena covers several design directions. Use models the user names; otherwise query families:

```python
runners = []
for q in ["opus", "gpt", "gemini"]:  # example family queries
    ms = await rlm.find_models(q, limit=1)
    if ms:
        runners.append(f"{ms[0]['provider']}/{ms[0]['id']}")
runners = runners or [None]  # None: the settings default
state["arena_runners"] = runners
```

   Say which families were not available.
4. Each candidate gets its own output: `worktree=True` gives it a private branch of your tree as it is now, per
   the separate-before-serializing-shared-state principle (`../principle-separate-before-serializing-shared-state/SKILL.md`). Outside Git, use `worktree="auto"` and give each brief its own
   directory (`/tmp/arena-<slug>/candidate-<n>/`).

## Phase B: Fan out

Spawn all N at once, each with the task, the paths to the shared grounding, and instructions to produce the artifact
plus a short rationale naming the alternatives it considered and what it rejected.

```python
TASK = "..."  # the contract: artifact, constraints, grounding paths
FINISH = """
When done: await rlm.finish("passed", summary, evidence=[<commands run with outcomes, or files with lines>],
outputs={"rationale": "<alternatives considered, what you rejected and why>"}, changed_files=[...]).
Do not commit; your branch is committed for you."""

hs = await asyncio.gather(*[
    rlm.spawn(TASK + FINISH, name=f"arena-{i}", model=m, worktree=True) for i, m in enumerate(runners)
])
results = await rlm.collect(hs)
cands = []
for i, (m, item) in enumerate(zip(runners, results)):
    r = item["result"]
    wt = r.get("worktree") or {}
    if r.get("status") != "succeeded" or not wt.get("commit"):
        print(f"candidate {i} ({m}) dropped: {r.get('status')} {r.get('error', '')}")
        continue
    diff = await bash(f"git -C {wt['path']} diff {wt['base']} {wt['commit']}")
    rationale = ((r.get("verdict") or {}).get("outputs") or {}).get("rationale", "(none)")
    cands.append({"label": f"candidate-{i}", "model": m, "handle": hs[i], "worktree": wt, "diff": diff.output,
                  "rationale": rationale, "check": (r.get("check") or {}).get("outcome")})
state["arena"] = [{k: c[k] for k in ("label", "model", "check")} | {"branch": c["worktree"]["branch"]} for c in cands]
```

A candidate that produced nothing is a dropout: proceed with N-1 and note it in the synthesis record.

## Phase C: Cross-judge

After every candidate finished, one judge frame on a model family different from yours (and preferably from the
runners' families too) sees the rubric and the diffs by label, scores each criterion, and recommends a base. It is a
frame, not a subagent: it only judges the text it is given.

```python
judge_ms = await rlm.find_models("gpt", limit=1)  # a family other than yours
judge_model = f"{judge_ms[0]['provider']}/{judge_ms[0]['id']}" if judge_ms else None
RUBRIC = ["...", "..."]
verdict = await rlm.infer(
    "Score each candidate against every rubric criterion (1-5, one-line reason each), then recommend the base: the "
    "candidate a future maintainer can extend most easily without breaking invariants. Name what each other "
    "candidate does better than the base.",
    context=["Rubric:\n" + "\n".join(f"- {c}" for c in RUBRIC)]
            + [f"## {c['label']}\nRationale: {c['rationale']}\n{c['diff']}" for c in cands],
    contract={"type": "object", "properties": {
        "scores": {"type": "object"}, "base": {"type": "string"}, "why": {"type": "string"},
        "graft_candidates": {"type": "array", "items": {"type": "string"}}},
        "required": ["scores", "base", "why", "graft_candidates"]},
    model=judge_model,
)
print(verdict or "judge incomplete")
```

For large diffs, `h = await rlm.load(text=c["diff"])` and pass views instead of whole strings.

## Phase D: Pick a base

Read every candidate end to end before picking: its diff and its rationale (both in `cands`; the files themselves
under `c["worktree"]["path"]`). Do not trust the judge or the candidates' self-reports alone;
run the candidates' tests in their worktrees where it matters (`await bash(f"cd {c['worktree']['path']} && npm test")`).

Score each candidate against the rubric criterion by criterion, not on holistic feel. Compare with the judge.
Agreement on the base confirms the pick. Disagreement means one of you is biased or the rubric was ambiguous: read both
rationales before deciding.

Pick the base a future maintainer can extend most easily without breaking invariants. When two feel tied, prefer the
cleaner boundary or smaller API, per the laziness-protocol principle (`../principle-laziness-protocol/SKILL.md`).

## Phase E: Graft

Walk each losing candidate once more and name what is worth porting into the base. Usually one or two things per
candidate, not most of it.

Merge exactly one branch, the base, then fold each graft in by hand with `edit`, per
the redesign-from-first-principles principle (`../principle-redesign-from-first-principles/SKILL.md`). Do not merge or paste losing branches mechanically: the result has to stay
coherent under one mental model. When no candidate is a sound base, merge none and write the synthesis yourself from
what you learned.

```python
base = next(c for c in cands if c["label"] == "candidate-0")  # your pick
merged = await rlm.merge([base["handle"]])
print(merged["ok"], merged["results"])
# grafts: await edit(path=..., old_str=..., new_str=...) against the losers' diffs, by hand
# after grafting, discard exactly the losers (from the repo root you work in)
for c in cands:
    if c is not base:
        wt = c["worktree"]
        print(await bash(f"git worktree remove --force {wt['path']} && git branch -D {wt['branch']}"))
```

Record what was grafted, from which candidate, and what was rejected and why.

When the N candidates converge on the same shape, that is a strong agreement signal: note it and ship the consensus
shape with no graft. When they wildly diverge, Phase A was under-specified: reframe and re-run rather than averaging the
divergence.

## Phase F: Verify

The synthesized artifact faces the same scrutiny as any other output, per the prove-it-works principle (`../principle-prove-it-works/SKILL.md`): run the checks on
the merged tree and read the result.

If verification finds a problem the arena did not catch, either Phase A was wrong (reframe and re-run) or a candidate
caught it and you missed the graft (back to Phase E). Do not paper over.

## Outputs

One synthesized artifact, uncommitted in your tree. One short synthesis note alongside it naming the base, the judge's
verdict, the grafts (with source candidate), the rejections, the dropouts, and the verification result.
