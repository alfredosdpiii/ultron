### Opening a PR

Run at the end of every other playbook that changes code. The repo's own rules (AGENTS.md, CONTRIBUTING, commit
conventions) win over this file where they differ.

**Worktree.** Work from a git worktree off main, not a shared checkout other sessions use. Children that edit files get
their own (`worktree=True`) and come back through `rlm.merge`. A branch dirty with unrelated work: export your patch,
start a fresh worktree, apply it there. A snarled worktree: start again from main and redo minimally.

**Commits.** Commit liberally, staging explicit paths. Rebase into small, ordered commits before the PR. Each commit is
a future PR: landable, ordered to tell the story. Amend when the fix belongs in the commit just made; a new commit when
separable.

**Before the PR.** Strip slop from the diff, then `../no-comments/SKILL.md`. Run `/review` on the diff (or
`r = await review_api.run(rlm, "main")` after `import review_api`, then `print(r.report)`) and fix what holds up. Write
the title, description and commit bodies with `../technical-writing/SKILL.md`, then `../unslop/SKILL.md`. Use one word
for each action, keep articles, prefer a plain verb over `-ing`.

**Titles.** Conventional Commits, `type(scope): subject`, unless the repo uses another form: `feat`, `fix`, `docs`,
`refactor`, `test`, `chore` or `perf`; the changed area as scope; a short imperative subject naming a real symbol when
one carries the change; no trailing period.

**Descriptions.** The body is a briefing, not the lab notebook. A reviewer with the diff learns why the change exists,
what it leaves out, what it could break and how you proved it, in under a minute. Short sentences, few identifiers,
under about 40 lines (it becomes the squash commit body). `##` headings, in this order, each dropped when empty:

- `## Why`: the problem and the approach in one to three sentences. No SHAs, no rebase genealogy.
- `## What changed`: one to three bullets. Name a symbol or path only when it carries the change; both sides of a rename.
- `## Scope`: what the PR covers and what it deliberately leaves out. One to three items.
- `## Tradeoffs`: only rejected alternatives a reviewer would ask about.
- `## Blast Radius`: one or two sentences on who or what it touches and why that is safe or risky. If main is red, the
  cost of leaving it red.
- `## Verification`: one to three bullets, each a real run and its outcome. For perf, one primary number as
  `before → after` with its unit; link the arena or swarm directory for the rest.

Attach screenshots or recordings when they prove a claim. No full SHAs, lane recitals, file-by-file checklists or
"CLEAN" verdicts; put such detail in a linked artifact. A commit body does not restate its subject.

**Forge.** GitHub through `gh` via `bash`. Write bodies to a file and pass `--body-file`.

**Size and stacks.** Prefer five narrow PRs to one large one. A stack is a base-branch chain: the root PR targets trunk;
each child branch is rebased onto its parent's exact tip and its PR targets the parent branch
(`gh pr create --base <parent-branch>`, `gh pr edit <pr> --base <parent-branch>`). Branch from trunk only for
independent work. Rebase on trunk before substantial stack work.

**Readiness.** Open every PR ready, never draft (omit `--draft`; `gh pr ready <number>` if one opens as draft). Run
`gh pr view <number>` before you refer to its status.

**Babysit.** Opening a PR does not start a babysit. Post the URL and keep building; finish the phase or stack first.
Babysit only when the user asks, once the stack exists (`playbooks/babysit.md`); a babysit per new PR stalls the build
and spends checks on commits later waves restart. Push back when feedback drifts from intent.

A child that opens a PR runs `../interrogate/SKILL.md` when the design is contested, the pre-PR steps above, posts the
URL in its `rlm.finish` outputs, and returns without babysitting, unless it is an Orchestrate PR owner whose brief
assigns the babysit loop (`playbooks/orchestrate.md`).
