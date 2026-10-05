# pstack for Ultron

These skills are ported from [pstack](https://github.com/cursor/plugins/tree/main/pstack) by Lauren Tan
([poteto](https://x.com/poteto)), the engineering skills she uses to ship high-quality code. The judgment is hers; the
port changes the mechanics so they run in Ultron's REPL: subagents are `rlm.spawn` children with checked verdicts,
judges and reviewers are `rlm.infer` frames, model panels come from `rlm.find_models`, long runs are `/goal`, and
nothing polls or spawns a subagent just to read files.

pstack is MIT licensed; its license is [LICENSE](LICENSE) in this directory, and each ported `SKILL.md` names its
source. Changes from the original: Cursor-only mechanics replaced or removed, `poteto-mode` renamed `rigor`, the
three orchestration playbooks merged into one, and `setup-pstack`, `poteto-help` and `make-bot-ui` left out (Ultron's
model settings and `/skill:` commands cover the first two; the third needs Grok Bot).

## Use

- `/skill:rigor <task>`: the rigorous mode. It reads the task, picks a playbook (bug fix, feature, refactoring,
  perf, investigation, shipping, orchestrate, ...) and runs the other skills as the steps need them.
- `/skill:<name>` runs one skill directly: architect, arena, automate-me, benchmark-checklist, blast-radius, bro,
  correct, create-verification-skill, figure-it-out, how, interrogate, maintain-verification-skill, no-comments,
  recall, reflect, show-me-your-work, swarm, tdd, teach, technical-writing, typescript-best-practices, unslop, why.
- The 24 engineering principles are skills too (`/skill:principle-fix-root-causes`, `principle-prove-it-works`, ...);
  the other skills read the ones a step needs.

Most skills only load when asked for (`disable-model-invocation`), so they cost nothing until used.

## Override or turn off

A skill with the same name in `~/.ultron/agent/skills/` or a project's `.pi/skills/` or `.agents/skills/` replaces the
bundled one. `ULTRON_BUNDLED_SKILLS=off` leaves all bundled skills out, and so does `--no-skills`.
