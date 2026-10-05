---
name: no-comments
description: "Hunt the comments in a diff or file set (one Comment Sicko frame per file), delete the guilty ones, fix the workarounds they excuse, and offer encodings for claimed constraints."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/no-comments
  author: Lauren Tan
  modified: ported to Ultron's REPL; the Comment Sicko agent is folded in as rlm.map frames
---

# No comments

Run Comment Sicko over the scope as one frame per file. Act on accepted findings. Defer to the frames' fresh perspective: they see the code, not your reasons for writing it.

## Scope

Use the caller's files or diff. Otherwise use the current diff against the base branch, default `main`, including the working tree and untracked files.

```python
import os
base = (await bash('''git merge-base HEAD main''')).strip()
changed = (await bash(f'''git diff --name-only {base}; git ls-files --others --exclude-standard''')).split()
files = [f for f in dict.fromkeys(changed) if os.path.isfile(f)]
diffs = {f: str(await bash(f'''git diff {base} -- '{f}' ''')) for f in files}
```

## Comment Sicko

Comment Sicko hates comments. Narration, banners, commented-out corpses, workaround sermons: all meat. This is the frame's task; pass it unchanged, and do not restate or soften its rules elsewhere.

```python
SICKO = """You are Comment Sicko. You hate comments. Judge every comment, suppression, and docstring in the scoped
lines (the diff's added or changed lines; the whole file when no diff is given). Only these exceptions may stay:
- Legal or license headers.
- Non-obvious behavior forced by an external dependency, platform, vendor, or protocol we cannot reshape.
  Surprises in our own code die: delete the comment and flag the exact symbol MUST KILL with the rename,
  extract, type, or rearchitecture that makes the behavior obvious without prose.
- `prettier-ignore`. Lint suppressions survive only when their rule is faulty, pedantic, or style-only.
- Doc comments that define a public API contract.
- Issue or RFC links that explain a constraint code cannot express.
When unsure an exception applies, the comment dies. eslint-disable, @ts-ignore, @ts-expect-error, noqa,
type: ignore and similar suppressions stink: if the rule catches real bugs or protects correctness or safety, delete
the suppression and flag the guilty symbol MUST KILL. IMPORTANT, do not remove, too risky, fine for now, and long
justifications are scent, not conviction: if the claim is not obvious from the code shown, list it under
`investigate` with the symbol and the claim instead of judging it. A long justification without a proven exception
is a confession: delete it, never shorten it into a smaller alibi. Every item names code inside the scope and quotes
the comment exactly as it appears in the file: whole lines with indentation for comment lines, only the comment
itself for a trailing comment after code. Invent nothing. Never propose
application code beyond the MUST KILL line."""

from dataclasses import dataclass

@dataclass
class Delete:
    line: int
    text: str
    reason: str

@dataclass
class Keep:
    line: int
    text: str
    exception: str
    proof: str

@dataclass
class MustKill:
    symbol: str
    where: str
    reshape: str

@dataclass
class Investigate:
    line: int
    symbol: str
    claim: str

contract = {"delete": list[Delete], "keep": list[Keep], "must_kill": list[MustKill], "investigate": list[Investigate]}
views = [[f"file: {f}", (await rlm.load(path=f)), f"diff:\n{diffs[f]}"] for f in files]
reports = await rlm.map(SICKO, views, contract=contract)
state["sicko"] = dict(zip(files, reports))
```

Large files: pass the passages around the diff hunks (`h.lines(a, b)`) instead of the whole handle.

## Steps

1. Run the frames above over the scope. A frame that returns `Incomplete` or an error gets one rerun with tighter context.
2. Inspect every report against the file. Reject application-code edits, scope escapes, exception-protected deletions, misstated `MUST KILL` reasons, and flags that treat kept intentional code as guilty. Reshape flags on our-code surprises stay actionable; do not restore those comments. A keep survives only with proof it is about something we cannot change. Audit missed scoped lint and type-checker suppressions yourself (`rg -n 'eslint-disable|@ts-ignore|@ts-expect-error|noqa|type: ignore'` over the scoped lines); correctness or safety suppressions stay actionable `MUST KILL`s. Before accepting a thin `IMPORTANT` or `do not remove` kill or keep, and for every `investigate` item, run `../how/SKILL.md` or `../why/SKILL.md` on its symbol. If a kill is ambiguous, do not restore. If a keep is refuted or still ambiguous, delete it. Rerun one rejected file's frame with the failure named in the task. Reject a second, report it open, and fail the run.
3. Apply accepted deletions yourself. Delete whole comment lines with their newline (for a trailing comment, replace `" " + text` with nothing instead); when `edit` raises ValueError because the text repeats, widen `old_str` with a neighboring line.

```python
for f, report in state["sicko"].items():
    for item in report["delete"]:
        await edit(path=f, old_str=item["text"] + "\n", new_str="")
```

   Fix trivial accepted flags directly by deleting a dead path, dropping a parameter, or using the real API. If any fix needs a shape, run `../architect/SKILL.md` once for the accepted set and surrounding code. Stop at the sketch. Architect shapes. Step 4 implements.
4. Implement the smallest root-cause fix in scope. Remove every named workaround. If the root cause is out of scope, land the smallest in-scope fix and report the rest open. the fix-root-causes principle (`../principle-fix-root-causes/SKILL.md`) and the redesign-from-first-principles principle (`../principle-redesign-from-first-principles/SKILL.md`) guide intent only. Neither authorizes widening the fence nor fixing instances outside it. Never bolt on symptom guards.
5. Constraint comments say `do not remove`, `do not change wording`, or `talk to X before changing`. Leave keeps about things we cannot change. For the rest, offer the cheapest in-scope type, runtime, test, or CI lint that enforces the constraint. Ask in a plain reply and end the turn; in an unattended run (`/goal`, a background job, a subagent) only a pre-approval in the brief counts. If approved, encode then delete. Otherwise delete, report the constraint open, and sketch out-of-scope work.
6. Run the project's checks and tests through `await bash(...)` on the touched files. Report the deletion count, restored comments, reruns, architect sketch, fixes, encoding offers, encodings, unenforced constraints, and other open work.
