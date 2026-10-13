# Incident and postmortem context

- **Issue tracker**: tickets labelled `incident`, `sev-*`, `postmortem-action-item`, `reliability`.
- **Team chat**: incident channels (`#incident-*`, `#sev-*`) around the dates the target code was added.
- **Source control**: commits like "fix for incident", "add defensive check", or a revert followed by "re-apply with ..."; `git log -i --grep=incident --grep=postmortem --grep=sev -- <file>`.
