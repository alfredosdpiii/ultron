---
name: correct
description: "Find the mistakes agents keep repeating in this repo and make each one impossible. Try architecture first, then types, then a lint whose error names the fix, then a test, and write docs last. Prove each check fails on a real past mistake. Repeat this each time the operator corrects you. Use for /skill:correct."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/correct
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Correct

The operator keeps correcting agents in this repo for the same mistakes. Change the repo so the next agent can't make them.

Assume every contributor is an agent that sees only the files it opened, copies the nearest example, and takes the shortest path that compiles. Design the repo so a change that looks right from one file is right for the whole repo.

## Find the mistake classes

First, read recent commits, reverts, review comments, agent instruction files (`AGENTS.md`, `CLAUDE.md`, `.pi/skills/`, `.agents/skills/`), and comments that explain workarounds. Narrow with code, then read the hits:

```python
log = await bash('''git log --since="3 months ago" --format='%h %s' | rg -i 'revert|fix(up)?|again|actually|oops|wrong|restore|undo' | head -80''')
reverts = await bash('''git log --since="3 months ago" --format='%h %s' --grep='^Revert' | head -40''')
rules = await bash('''git log -p --since="3 months ago" -- AGENTS.md CLAUDE.md | rg '^[+-][^+-]' | head -120''')
workarounds = await bash('''rg -n -i 'workaround|hack|do not|don.t use|instead of|must not|IMPORTANT' --glob '!node_modules' | head -80''')
prs = await bash('''gh pr list --state merged --limit 30''')
print(log, reverts, rules, workarounds, prs, sep="\n---\n")
```

Read review comments on the PRs that look like corrections with `gh pr view <n> --comments`.

Group the mistakes into classes in `state["classes"]`, each with the commits or lines that show it. A class counts once it has happened twice. When the history is long, read candidate diffs with `git show <sha>` yourself; use `rlm.map` over diff chunks only for the ones a line or two cannot classify.

## Fix each class at the highest level that works

1. **Eliminate it with architecture.** Give each piece of state one owner and each task one supported way. Hide internals so the wrong import fails. Replace hand-synced lists with one source of truth. Delete old ways and dead code an agent would copy.
2. **Enforce it with types so the bad state can't be written.** If bad code still compiles, add a lint or CI check whose error names the file, type, or function to use instead. If the pattern is already common, fail only when a change adds more.
3. **Test the behavior.** Fix or delete any test that would still pass if every function it calls returned nothing.
4. **Write docs or agent rules last, only for judgment calls.** A line in `AGENTS.md` or a project skill. Nothing fails when an agent skips them.

When the repeated mistake is in how the agent itself works (a procedure it keeps re-deriving wrong, not a property of the repo's code), the fix between levels 3 and 4 is a tested code skill: `await skills.propose_code(name, source, test_source, evidence)` runs `test_source` in a fresh kernel and activates the skill only if it passes; later sessions use it with `from code_skills import <name>`. `await skills.code_list()` shows what already exists, `await skills.code_history(name)` its versions, `await skills.rollback(name, version)` undoes one. A code skill only helps agents that know to import it, so it never replaces a repo-level check.

## Fix and prove

Then fix the most frequent classes now, one commit each. Prove each new check fails on a real past mistake: check out or re-apply the offending change in a scratch worktree, run the check through `await bash(...)`, and keep the failing output as evidence. Run the same command locally and in CI. Exceptions go on the offending line with a reason, an expiry date, and a human's approval.

## Keep the rule table

Last, keep a table in the agent instruction file that pairs each rule with what enforces it. When the operator corrects you, fix the mistake and add the rule. If the rule was already there and nothing enforces it, that's a repeat, so fix it at the highest level in the same change. Drop a rule once its mistake can't happen.

**Reply:** each class with its evidence, the level you picked, and why a higher level didn't work.
