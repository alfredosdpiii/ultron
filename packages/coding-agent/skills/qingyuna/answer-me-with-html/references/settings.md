# Settings, clean-up and updates

Read this when SKILL.md section 0 sends you here.

When the arguments for this call (the text after the skill block, see section 0 of SKILL.md) start with `config` (for example `/skill:answer-me-with-html config open off`), this turn handles settings only and produces no page:

- `config`: run `am config` to show the current settings, then let the user choose. Ask in plain text, at most 4 settings at a time, these first: `open`, `theme`, `mode`, `style`, with the current value marked.
- `config <key> <value>`: run `am config set <key> <value>`.
- `config reset [key]`: run `am config reset [key]`.

When the user asks in natural language ("stop opening the browser", "use the card theme by default"), also convert it to `am config set`. Settings: `open` (auto-open the browser), `theme`, `mode`, `style`, `voice` (video narration). Leave `update_check` alone: Ultron runs the CLI with `AM_NO_UPDATE_CHECK=1`. Run `am config` to see all descriptions.

When the arguments start with `clean`, or the user asks to clean up pages / the cache: first run `am clean --dry-run` and tell the user how many items and how much space will be deleted. Run `am clean` only after the user agrees (add `--all` to delete all pages and videos, `--days N` to change how many days to keep).

When the arguments start with `update`, or the user asks to update this skill: this copy ships with Ultron and updates with it. Tell the user to update Ultron (`npm install -g ultron-agent`); do not run `npx skills`, `claude plugin` or `git pull` for it.
