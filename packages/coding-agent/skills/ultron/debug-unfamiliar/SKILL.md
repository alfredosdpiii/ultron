---
name: debug-unfamiliar
description: "Facts-first debugging for code nobody here knows well. Gathers facts with their provenance, compares the broken case against a control, ranks hypotheses in a hypotheses-by-facts matrix, descends one layer at a time for a stronger signal, and accepts a cause only when it fits every fact. Use for /skill:debug-unfamiliar, regressions, latency, leaks, hangs, or intermittent failures where reading the code has not explained the bug."
disable-model-invocation: true
metadata:
  source: Ultron; the method follows Cindy Sridharan's "How to effectively debug when you no longer understand the code" (copyconstruct, 2026)
---

# Debug unfamiliar code

Code tells you what can happen, not what did happen. A cause you infer from reading code is a guess you can make
sound convincing, and neither you nor the user may know the code well enough to catch it. So this skill works from
facts: you collect them, every hypothesis must fit all of them, and you read code only at the end to locate a cause
that the facts have already pinned down.

Two tiers, kept apart:

- **Facts**: logs, traces, profiles, packet captures, configs, flag values, versions, git history, user reports,
  measurements. You collect and analyze them; you never edit or argue them away. If one looks wrong, re-gather it or
  ask whoever gathered it.
- **Hypotheses**: everything you infer, including a reproduction. A hypothesis is valid only if no fact contradicts
  it.

Keep everything in `state["debug"]` so it survives a kernel restart:

```python
d = state.setdefault("debug", {"symptom": None, "variable": None, "control": None,
                               "facts": [], "hypotheses": [], "layer": 1, "interventions": []})
def fact(id, text, source, how, layer, in_variable=True, in_control=None):
    # in_control: True (also seen in the control), False (absent there), None (not checked yet)
    d["facts"].append(dict(id=id, text=text, source=source, how=how, layer=layer,
                           in_variable=in_variable, in_control=in_control))
```

## 1. Frame the bug, without reading code

Write down the symptom, where it happens (the **variable**), when it started, the last known good version, and what
the report already contains. Record everything from the report as facts first: a report often carries the
constraints that decide the case (a version range, a default setting, a frame in a heap profile).

Gather read-only. Ask before anything that changes state or touches production beyond reading (restarts, flag flips,
SSH into a production host); the user decides access.

## 2. Pick a control and isolate signals

A **control** matches the variable except for the suspected difference: the same build with the flag off, the last
good version, a healthy host. When the happy path differs in too many ways, use a **sad path** instead, a case that
fails loudly for a different, well-understood reason (a permission error instead of a silent hang). A control is
only useful if you are confident it is not broken the same way.

Measure the same things on both. Prefer counts and sizes you can compare (addresses returned, bytes, files open,
requests sent, time to first byte):

- in the variable, not the control: **signal**. Follow it.
- in both: **noise**. It points nowhere, but it can still rule a hypothesis out.

When several reports exist, a pattern that repeats across them and is missing from the control is a signal, not a
coincidence.

## 3. Rank hypotheses with a matrix

Generate hypotheses only from signals. Then build the matrix and read it both ways:

```python
H = d["hypotheses"]   # [{"id": "H1", "text": ..., "fits": {fact_id: True/False/None}}]
F = [f for f in d["facts"]]
signals = [f for f in F if f["in_variable"] and f["in_control"] is False]
strength = {s["id"]: sum(h["fits"].get(s["id"]) is True for h in H) for s in signals}   # by column
survivors = [h["id"] for h in H if all(h["fits"].get(f["id"]) is not False for f in F)]  # by row
rows = [[h["id"]] + ["✓" if h["fits"].get(f["id"]) else "✗" if h["fits"].get(f["id"]) is False else "?"
         for f in F] for h in H]
print(strength, survivors)
```

- **By column**: a signal that only one or two hypotheses fit is **strong**; follow those. A signal that fits
  almost everything is weak. Aggregate symptoms (P99 latency, error rate, SLO burn) are usually weak: they are what
  the bug looks like, not where it is.
- **By row**: if no hypothesis fits every fact, try these in order before inventing a new one:
  1. Re-check how the suspect facts were gathered (wrong host, wrong time window, wrong tool version).
  2. Split a hypothesis that bundles two ideas, such as "the release caused it" when the release had two changes.
  3. Merge narrow per-instance hypotheses that each explain one host into one shared cause that explains them all.
  4. Combine two hypotheses that each explain different facts and contradict none. Each part must be necessary:
     remove it and some fact is left unexplained.
  5. Only then form a new hypothesis, from the facts you have.

Show the user the matrix as a table whenever it changes the ranking.

## 4. No strong signal: go one layer down

If every signal is weak, or every hypothesis still fails after step 3, don't generate more hypotheses. Gather facts
from the next layer and repeat steps 2–3. Start with what you already have or can get cheaply, and stop at the
first strong signal, not the deepest:

| Layer | Typical facts |
| --- | --- |
| 1. Report and surface | the report, logs, metrics, config, flags, versions, git history, earlier postmortems |
| 2. Service and network | per-request traces, DNS answers, connection and handshake timing, packet captures |
| 3. Process | stacks, thread states, heap or CPU profiles, open files and sockets, syscall traces |
| 4. Kernel and OS | IPC and port tables, scheduler and memory stats, kernel logs |

Write a throwaway script for any layer whose format is new to you; that is cheaper than reading about it. Record
`d["layer"]` and why you descended.

## 5. Validate the survivor

- **Every fact**, noise included. A single contradicting fact discards the hypothesis; for a combined hypothesis it
  shows which part fails. Remove each part in turn to prove it is necessary.
- **A reproduction is a hypothesis.** It counts only if it produces the strongest signal from the real reports (the
  same frame in the profile, the same packet sequence, the same held IPC reply). A repro that shows the symptom but
  not the signal is a different bug; drop it from the investigation.
- **Review to eliminate, not to confirm.** One reviewer frame on a model family different from yours, asked to find
  a fact the hypothesis fails, a control broken the same way as the variable, or a fact gathered badly. A reviewer
  can't see facts nobody gathered, so its agreement proves nothing; only its contradictions count. Require a fact
  id for every claim:

```python
import json
ms = await rlm.find_models("gpt", limit=1)   # a family other than yours
reviewer = f"{ms[0]['provider']}/{ms[0]['id']}" if ms else None
verdict = await rlm.infer(
    "Try to eliminate the hypothesis. Find a fact it contradicts, a signal that also appears in the control, a control "
    "that may be broken the same way as the variable, or a fact that may have been gathered from the wrong host, "
    "window or tool version. Cite fact ids for every claim; uncited claims are discarded. If nothing eliminates it, "
    "say which facts are missing that could.",
    context=[json.dumps(d["facts"], indent=1), json.dumps(hypothesis, indent=1)],
    contract={"eliminated": bool, "contradictions": list[str], "suspect_facts": list[str], "missing_facts": list[str]},
    model=reviewer,
)
```

Treat `missing_facts` as the next facts to gather, not as a pass.

## 6. Now read the code, fix, and prove it

With the cause pinned down, read just the code that produces it and make the smallest fix. Prove the fix the way
you found the bug: the strong signal disappears in the variable and the control is unchanged. Passing tests alone
don't prove it; the original tests passed with the bug in.

Then make the signal cheap next time: log it, count it, or add a test or gate that compares it against a known good
run (for example, DNS answers per client before and after a release), so the next investigation finds it at layer 1.

## Decisions and handoff

Some calls belong to the user: which of several fixes to ship, rolling back, escalating to another team. Present
each option with its tradeoff and your recommendation. Every time the user has to supply a control, a constraint or
a check, record it in `d["interventions"]`: it shows where the process needed a human.

Report, briefly:

- the cause, with the fact ids that prove it;
- the final matrix;
- the control used and how the cause was validated, including the reviewer's result;
- the fix and how the signal check proves it;
- the facts still missing and any open decisions.
