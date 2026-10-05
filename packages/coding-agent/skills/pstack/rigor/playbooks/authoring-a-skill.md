### Authoring or modifying a skill

**You own the skill's voice.**

1. Pick the form. Judgment and procedure the model reads is a SKILL.md, per Ultron's `docs/skills.md` (frontmatter:
   `name` matching the directory, a `description` that routes, `disable-model-invocation` when it should load only on
   request). A procedure that worked and will recur as code is a tested code skill instead:
   `await skills.propose_code(name, source, test_source, evidence)` (`help(skills)`), reused with
   `from code_skills import <name>`.
2. Validate: frontmatter parses with `name` and `description`, referenced files exist, cross-skill links resolve,
   every Python block compiles. Load it once with `/skill:<name>` (after `/reload`) and read the startup diagnostics.
3. Test cases if the skill is structural; skip if subjective. For a behavior change worth proving, run
   `playbooks/eval.md`.
4. Run `playbooks/opening-a-pr.md`.

When in doubt, delete. Keep only prose that changes a decision. Tell it to do the thing; explain only when the rule is
confusing without a reason. Match tone to scope. Point at structural sources (types, READMEs, config) instead of
restating them (`the encode-lessons-in-structure principle (`../principle-encode-lessons-in-structure/SKILL.md`)`). Delegate to other skills by path. A workflow you keep
hitting that no skill captures: propose a new one.

**Reply:** what the skill does, key design decisions, validation notes.
