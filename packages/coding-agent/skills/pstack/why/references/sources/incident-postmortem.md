# Incident and postmortem context

Not a separate source: a cross-cutting angle. Incidents often motivate defensive code ("we added this check after the X outage"). When the target looks defensive (null checks, retries, timeouts, rate limits, feature flags), add incident queries to every source you search:

- **Source control**: commits like "fix for incident", "add defensive check", or a revert followed by "re-apply with ..."; `git log -i --grep=incident --grep=postmortem --grep=sev -- <file>`.
- **Issue tracker**: tickets labelled `incident`, `sev-*`, `postmortem-action-item`, `reliability`.
- **Documents**: postmortems mentioning the target file, feature or error string.
- **Team chat**: incident channels (`#incident-*`, `#sev-*`) around the dates the target code was added.
- **Observability**: formal incident records with timelines; dashboards and monitors created as postmortem action items.
- **Error tracking**: issues whose first-seen / last-seen window brackets the target's ship date, with stack traces through the target.
- **Analytics warehouse**: an error-classifying event that spikes in the incident window and drops after the fix ships. Circumstantial support that the code resolved the user-visible symptom.

If you find an incident link, fetch the full postmortem: its action items usually tie directly to code changes. When sources corroborate each other (an incident ID in a ticket, the ticket in a postmortem, the postmortem linking the PR, the error count dropping after the merge), the evidence is especially strong.

Worth the time only when the code's defensive character makes an incident origin plausible.
