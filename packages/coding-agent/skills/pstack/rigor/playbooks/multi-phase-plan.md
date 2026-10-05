### Multi-phase or multi-PR plan

**You own the plan, not the code. The plan is a checklist an owner runs box by box and the operator audits from the
evidence.** The plan is the deliverable. Do not implement.

1. When the change is one or two files with an obvious approach, skip the plan. Say so and stop.
2. Settle open questions by prototype before writing (`playbooks/prototype.md` for each). Keep the branch, SHA and
   screenshots for Appendix A. Ask the operator only about product or preference calls no run settles, with options
   (`the never-block-on-the-human principle (`../principle-never-block-on-the-human/SKILL.md`)`).
3. Explore with code first (`rg`, `rlm.load` handles); spawn read-only children only for independent multi-step
   exploration, each returning file pointers, conventions, test commands and entry points, no dumps
   (`the guard-the-context-window principle (`../principle-guard-the-context-window/SKILL.md`)`).
4. Copy the skeleton below into the plan file and fill every placeholder. Unless the operator names a path, write it
   to `.orchestrate/<slug>-plan.md` (kept out of version control). Keep every heading and sub-block in order. One
   section per PR; one PR is one change with its own evidence (`the sequence-verifiable-units principle (`../principle-sequence-verifiable-units/SKILL.md`)`). The execution
   playbook is `playbooks/orchestrate.md`; name its delivery shape (Full, Stack or Program) per that playbook's rule.
5. Write under `../technical-writing/SKILL.md` in full, then `../unslop/SKILL.md`. The body is one Diátaxis mode,
   how-to; appendices hold explanation and reference. Each heading states the task or the finding.
6. Check the plan's structure in the REPL and fix every line it prints
   (`the encode-lessons-in-structure principle (`../principle-encode-lessons-in-structure/SKILL.md`)`):

```python
import re

plan = await read(".orchestrate/<slug>-plan.md")
problems = []
fence = "`" * 3
prose = re.sub(fence + ".*?" + fence, "", plan, flags=re.S)
for n, line in enumerate(prose.splitlines(), 1):
    if re.search("[" + chr(0x2013) + chr(0x2014) + "]", line):
        problems.append(f"{n}: long dash")
    if re.search("[" + chr(0x2018) + chr(0x2019) + chr(0x201C) + chr(0x201D) + "]", line):
        problems.append(f"{n}: curly quote")
for heading in ["## How to read this", "## Program checklist", "## Close the program", "Prototype evidence"]:
    if heading not in plan:
        problems.append(f"missing {heading!r}")
blocks = ["**Depends on.**", "**Files.**", "**Build.**", "**You see.**", "**Verify, unit.**", "**Verify, live.**",
          "**Verify, perf.**", "**Review gate.**", "**Merge.**"]
for section in re.split(r"\n(?=## )", plan):
    if not re.match(r"## .+\(.+\)", section):
        continue
    title = section.splitlines()[0]
    found = [b for b in blocks if b in section]
    if found != blocks:
        problems.append(f"{title}: sub-blocks {found}")
    lanes = re.findall(r"- \[ \] Lane (\d+)\..*", section)
    if lanes != [str(i) for i in range(1, 11)]:
        problems.append(f"{title}: lanes {lanes}")
    for lane in re.findall(r"- \[ \] Lane \d+\..*", section):
        if "Save `" not in lane or "Pass when" not in lane:
            problems.append(f"{title}: lane lacks a screenshot or pass predicate: {lane[:60]}")
print("\n".join(problems) or "plan ok")
```

   Also reread the prose for mid-sentence colons; the script does not catch them.
7. Hand back the plan path and the check output, then stop. Execution starts on the operator's explicit go.

**Verification.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf
boxes are all checked (`the prove-it-works principle (`../principle-prove-it-works/SKILL.md`)`). Every verification block opens with that rule. The live block is
mandatory: ten lanes at the PR head drive the real surface, per `../swarm/SKILL.md`, on the `code` model. Each lane is
one box with a concrete scenario, the screenshot or captured output it saves, and its pass predicate. Lane 1 is the
regression lane against trunk: the same load-bearing scenario on trunk and head; if trunk lacks the feature, it records
that and gates the added behavior plus the end state the user waits for. The perf gate is dual-sided: trunk and head
both produce the named metric; if trunk lacks the feature, isolate the added work and set an absolute budget for it and
for the end state. No ratios between unlike scenarios. A PR that changes an interaction is review-gated: the operator
reviews screenshots and a recording before merge. A PR that changes none writes
`**Review gate.** None. <PR id> is not review-gated.` with no boxes.

**Surface.** CLIs and TUIs are driven through tmux; web UIs through a configured browser MCP server. A PR touching two
surfaces gets lanes on both. A surface with no driver is a risk in Appendix C, and its live block still says how each
lane drives it.

````markdown
# <Program> plan

<Under ten lines. What changes, for whom, the rule the program enforces, and the PR ids in order.>

## How to read this

One box is one unit of work. Every box names the evidence that checks it. A nested box is a sub-step of the box above
it. Check a box only when its evidence exists, a file, a log line, a screenshot, a test run, or a SHA. The body is a
how-to. The appendices explain and record.

The program runs the rigor skill's `playbooks/orchestrate.md` in the <Full | Stack | Program> shape. <Who merges, and
which PR ids are the operator's items that stop at merge-ready.>

Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

## Program checklist

### Arm the program

- [ ] State the protocol and this plan to the operator, then stop. Start only on the operator's explicit go.
- [ ] Read these at program start and again at every drain that starts a wave.
  - [ ] The rigor skill's `playbooks/orchestrate.md` and `playbooks/opening-a-pr.md`.
  - [ ] `../swarm/SKILL.md` and <each other skill the program uses>.
- [ ] Write the standing orders and seed the orchestration state.
- [ ] On the operator's hold or stand-down, send every owner a zero-writes order at once.

### Spawn owners

- [ ] Spawn one owner per PR with the lifecycle the delivery shape names.
- [ ] Follow this dependency graph. Start dependent work only after its parent merges, or base it on the parent branch
  in the Stack shape.
  - [ ] <PR id> and <PR id> are independent and first. Both branch from `main`.
  - [ ] <PR id> after <PR id>.
- [ ] Hold the file boundaries. <PR id or class> touches only `<glob>`.
- [ ] Hold the review gate. <PR ids> change an interaction and wait for the operator's review before merge.

### PR mechanics, for every PR

- [ ] Open the PR ready, never draft, per `playbooks/opening-a-pr.md`. A stack child targets its parent branch.
- [ ] Run the repo's lint and typecheck before the PR-facing push. Push with hooks on.
- [ ] Strip slop before each commit and run `../no-comments/SKILL.md` before review.
- [ ] Triage every review-bot and security-review comment per `playbooks/babysit.md`.
- [ ] Rebase onto current trunk before the code-ready report. Keep that merge base in fix rounds.

### Verdict and merge, for every PR

- [ ] At the code-ready head and each later push that changes the patch, run the swarm. One gates lane, the ten live
  lanes, the perf lane, and two or more audit lanes with their own focus that distrust the PR body.
- [ ] Clean only when every lane passes. Findings go back to the owner. A new head gets a fresh verdict unless the
  patch-id rule in `playbooks/shipping.md` keeps it.
- [ ] <The merge or append rule of the delivery shape.>

### Boot recipe, for every live lane

- [ ] `git fetch origin <head-branch> && git checkout <head SHA>` in the lane's own worktree.
- [ ] <Start the backend and the surface as a job. Wait for ready.>
- [ ] <Deliver input only through the surface driver. Name the read-only diagnostics.>
- [ ] Save every screenshot or capture to `/tmp/swarm-<pr-id>/lane-<n>/<slug>.png` and return the paths.

## <Task as a verb phrase> (<PR id>)

**Depends on.** <PR id, or None.>

**Files.**

- [ ] Edit `<path>`.

**Build.**

- [ ] <One change. Name the symbol and the file.>

**You see.**

- [ ] <One observable result, with the exact log line or screen state.>

**Verify, unit.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] <Test file and the case it gains.> Run `<command>`.

**Verify, live.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked. Ten lanes at the PR head, per the boot recipe.

- [ ] Lane 1. Regression lane against trunk. Run <the scenario> at trunk and head. Save `<slug>.png`. Pass when <predicate>.
- [ ] Lane 2. <Scenario.> Save `<slug>.png`. Pass when <predicate>.
- [ ] <Lanes 3 to 10 in the same form.>

**Verify, perf.** Tests alone are not sufficient verification. A PR is verified only when its unit, live, and perf boxes are all checked.

- [ ] Metric. <Measured at trunk and head.>
- [ ] Probe. <Command run at trunk and head, interleaved.>
- [ ] Baseline. Record the trunk <value> first.
- [ ] Rule. <Head against trunk, with the number that fails.>

**Review gate.** The operator reviews before merge.

- [ ] Post lane <n> screenshots and a 30 to 60 second recording in chat. Stop at merge-ready.

**Merge.**

- [ ] Clean verdict at the exact head SHA.
- [ ] Review-bot triage done.
- [ ] Rebased onto current trunk after the verdict, patch-id unchanged.
- [ ] <The owner squash-merges its PR, or the root appends it to the stack and the operator lands it.>

## Close the program

- [ ] Every box above is checked with its evidence.
- [ ] Reply to the operator with the report `playbooks/orchestrate.md` names.

## Appendix A. Prototype evidence

<Each question a prototype answered, with branch, SHA and artifacts. Each question that stays unproven.>

## Appendix B. Alternatives rejected

<Each approach weighed and why it lost.>

## Appendix C. Risks

<Each risk with the PR it lands in and what the owner watches.>

## Appendix D. Links and reading list

<Docs to read before editing. Which PRs get `../how/SKILL.md` and `../interrogate/SKILL.md`. The decision trail per
`../show-me-your-work/SKILL.md`.>
````

**Reply:** the plan path, the PR ids with their dependencies and the review-gated set, what the prototypes proved and
what stays unproven, and the check output.
