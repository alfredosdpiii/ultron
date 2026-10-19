---
name: verified-core
description: "Proved specs for small pure logic with sharp rules (parsers, money and date math, state machines, merge and replay, permission rules). The user states laws, Bend proves a reference implementation, and the real code is either the compiled Bend (JS/TS) or tested against it (any language). Falls back to Hypothesis or CrossHair when Bend is missing. Use for /skill:verified-core or when a bug in such logic would be expensive."
disable-model-invocation: true
metadata:
  source: Ultron; Bend 2 (bend-lang.com, github.com/bendlang/bend), law-driven development
---

# Verified core

A test checks the inputs someone thought of; a law holds for every input. This skill puts a proved specification
under the small parts of a program where a bug is expensive, and ties the shipped code to it.

## When to use it

Use it for pure logic with rules you can state precisely: parsers and serializers, money, date and unit arithmetic,
state machines, schedulers, merge, replay and dedup rules, permission and pricing rules. Never for glue, I/O, UI or
one-off scripts: ordinary tests are cheaper there. If the rules cannot be stated as laws, this skill does not apply.

## The laws are the user's

The proof is only as strong as the laws. A weak law ("the output is sorted") passes a wrong program (`[]`). So:

1. Write `LAWS.bend` first: each law one claim, with a comment saying it in words. Prefer laws that pin behaviour
   down completely (sorted AND a permutation; round-trip `parse(show(x)) == x`; last write wins AND unwritten keys
   keep their value).
2. Show the user the laws (the words, then the Bend) before proving anything, and get them confirmed unless the
   user already stated them. Never weaken a law to make a proof go through; say which law resisted and why.
3. After that, the user does not need to read the implementation or the proofs: the checker does.

## Setup

- `bend` (`~/.bend/bin/bend`, or on PATH). Read `bend guide` once per session before writing Bend; it is the
  language reference. Set `BEND_NO_TELEMETRY=1` for runs.
- Without bend: use the fallback below and say so. Do not install it unasked (its installer is `curl | sh`).
- Pin the Bend version in the project (it changes fast; 2.0.37 changed how Nat crosses to JS).

## Files

```
replay.bend   the reference implementation (pure defs only)
LAWS.bend     imports it and states the laws (human-owned)
PROOF.bend    imports LAWS.bend; `def L.<law>(...)` proves each law
```

The gate is `bend PROOF.bend --check-only`: it prints `ALL PROOFS CHECK` or `SOME PROOFS FAIL` with the expected and
observed terms. `--verdict` rechecks with a kernel proved in Lean (needs Lean 4.34); use it when the user asks for
the strongest check. `example/` holds a complete, checked case (a journal's last-write-wins replay) to copy from.

## Bend points that cost time

- Records stored in lists must be `Data`: declare `type Put is Data: Put{key: Nat, val: Nat}`; a pair type
  `Nat & Nat` is a `Type` and is refused in `List<&2, ...>`.
- A parameter used twice needs `+` (`+k: Nat`); laws too (`for +k: Nat`).
- Operators need a type: `(a + b : Nat)`. Equality of values is a call; `==` is only the proposition.
- No `if`: match on `True{}`/`False{}`. A `match` takes a parameter, never a computed value: pass the computed value
  to a helper that matches on its parameter (`pick(eqn(k, k2), v, rest)`).
- Recursion must shrink the first changing argument; put the shrinking parameter first.
- Rewrites: `%e : P` with `e : {a == b : T}` turns a goal `P[_ := b]` into `P[_ := a]`. To replace `f(x)` by
  `True{}`, state the lemma as `{True{} == f(x) : Bool}` (example: `L.eqn_refl`).
- Prove helper facts (like key equality being reflexive) as their own laws first.

## Mode 1: JS or TS project, ship the compiled core

`bend replay.bend -o replay.mjs` writes a standalone ES module (no imports). Import it and call any non-IO def:

- Constructors are `{$: "Name", field: value}` (`{$: "Con", head, tail}`, `{$: "Nil"}`, `{$: "Some", value}`).
- A `Nat` crosses as a whole JS number from 0 to 2^53 (larger or negative values throw); inside, Nat is capped at
  2^48 - 1. Validate inputs at the boundary.
- Arrays are passed without a copy and may be updated in place: copy one you keep.
- No types are generated: write a small `replay.d.ts` beside it for TS callers.
- Keep a thin TS wrapper that converts plain TS values to and from Bend values; test the wrapper with a few cases.
- The JS translation itself is not proved. Keep one end-to-end test through the wrapper.

## Mode 2: any language, Bend as the test oracle

The shipped code is written normally (Python, Rust, Go, ...). A differential test feeds the same generated inputs
to it and to the compiled oracle, and every answer must match:

- Live: a small `oracle.mjs` reads JSON cases on stdin and prints the oracle's answers (example: `oracle.mjs`,
  `test_journal_oracle.py`, 2000 generated cases per run).
- Fixtures: run the oracle once over generated cases and commit them as JSON; the tests replay them with no node or
  bend at test time. Regenerate when the laws change.
- C: `bend file.bend -o file.c` for languages with a C FFI when per-call speed matters.
- Generate inputs that reach the edges the laws mention (empty, repeated keys, maximum sizes) and keep values inside
  the Nat bounds above.

A passing differential test means the real code matched a proved spec on every generated input: strong evidence,
not a proof of the real code. Say it that way.

## Fallback without Bend

State the same laws as properties over the real code: Hypothesis (`@given`) for Python, fast-check for JS/TS,
proptest for Rust. For small pure Python functions, CrossHair (`crosshair check`) searches for counterexamples to
pre- and postconditions symbolically. For a proof about real Rust code, use Kani or Verus.

## Gate it

Make the checks part of the work's definition of done, and of a child's contract when delegating:

```python
rlm.spawn("implement the replay rules", contract={"checks": [
    {"run": "BEND_NO_TELEMETRY=1 bend PROOF.bend --check-only",
     "expect": {"stdout_contains": "ALL PROOFS CHECK"}},
    {"run": "python3 -m pytest -q test_journal_oracle.py"},
    {"changed_files_within": ["replay.bend", "PROOF.bend", "journal.py", "tests/**"]},
]}, hop={"retry": 1})
```

`bend` exits 1 when any proof fails. `changed_files_within` keeps `LAWS.bend` out of the child's reach: the laws are
the user's.

Report: which laws hold (`ALL PROOFS CHECK`), how the shipped code is tied to them (compiled, or N differential
cases), and anything outside the proof (I/O, the wrapper, the JS translation).
