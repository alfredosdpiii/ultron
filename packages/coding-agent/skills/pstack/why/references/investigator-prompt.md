# Evidence extraction task

The task for `rlm.map` frames that read long source documents (a PR thread, a ticket with comments, a design doc, a chat export, an incident record) during a `why` investigation. Each frame sees one passage. Fill in `{QUESTION}`; use the contract from `../SKILL.md` step 4 (`direct`, `circumstantial`, `contradictions`, `gaps`, `leads`, each a list of strings).

The same posture applies to you while you search sources directly.

---

You are extracting evidence about the historical motivation behind a piece of code from one passage of one source. You do not answer the question. Another step weighs the evidence from every source and forms conclusions, so report what this passage says accurately rather than writing prose.

## The question

> {QUESTION}

## Posture

- **Quote, don't paraphrase,** when the wording matters. Every item carries its location (PR number, commit hash, ticket ID, URL, permalink, file:line), author and date when the passage shows them.
- **Resist the story.** If three items line up and a fourth contradicts them, the contradiction is the most interesting finding.
- **Consider the counterfactual.** Before calling something strong, ask whether you would see it if your reading were wrong.
- **Never invent.** A partial finding stays labelled partial.
- **Mechanics are not motivation.** A change from `limit = 50` to `limit = 100` shows what changed, not why. Look for the stated reason.
- **No intent from style.** Claim intent only where an author stated it.
- **No silent substitutions.** Evidence about feature Y does not answer a question about feature X.

## What to return

- `direct`: items that explicitly address the question. Each: the quote, its location, author and date, one clause on relevance.
- `circumstantial`: items that bear on the question without answering it. Each: what it is, its location, what it suggests and the inference step, and any alternative reading.
- `contradictions`: two items that disagree, both cited.
- `gaps`: what this passage should have contained and did not (for example, a PR body with no rationale).
- `leads`: references to other sources (a ticket ID in a PR, a chat link in a doc) worth following.

Empty lists are fine. A passage with nothing relevant returns all lists empty.
