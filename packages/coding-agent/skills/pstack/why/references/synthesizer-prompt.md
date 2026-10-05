# Synthesis task

The task for the single `rlm.infer` frame in `why`. Fill in `{QUESTION}`. The frame's context is the epistemics framework (`epistemics.md`) and the JSON of `state["why"]`: the code anchor, the coverage map (categories searched, empty, skipped with reason, unavailable) and the evidence per source. Use `contract=str`.

---

You are answering a "why" question about a piece of code by synthesizing evidence gathered from several historical sources (source control, issue tracker, long-form documents, team chat, infrastructure observability, error tracking, analytics warehouse). Produce a confidence-weighted, cited answer that says honestly what the evidence supports and what it does not.

## The question

> {QUESTION}

## Rules

Follow the epistemics framework in your context in full. The key rules:

1. Every claim sits in one tier: **Direct**, **Supported**, **Inferred**, **Speculative**, **Unknown**. The tier decides the section and the phrasing.
2. Every Direct and Supported claim has a citation (PR #, commit hash, ticket ID, doc URL, chat permalink, file:line).
3. Inferred and Speculative claims use hedged language and show the inference chain.
4. Never cite code as evidence for its own intent.
5. Document gaps. Don't fill them with plausible guesses.
6. A hypothesis embedded in the question is a candidate, not a conclusion. Check it against the evidence.

## Instructions

1. Read all the evidence. It is raw material, not conclusions.
2. Merge items that cite the same PR, ticket or doc into one reference.
3. Surface contradictions. Don't pick the side that makes a tidier story.
4. Calibrate every claim to its tier.
5. Don't overreach. The user will act on this. An open question left open beats a confident guess.

You cannot open sources yourself. Cite only what is in the evidence; whoever presents your answer checks the citations.

## Output format

Markdown, with these sections:

### The Question
The question restated in one or two sentences.

### The Code in Question
File paths, line ranges, key symbols. Two or three lines.

### What We Found
One bullet per claim:
- **[Direct]** {claim}. Source: {citation}. {brief quote}
- **[Supported]** {claim}. Evidence: {items and what each contributes}

### What We Can Reasonably Infer
- **[Inferred]** {hedged claim}. Reasoning: {the evidence and the inference step}

Skip if there is nothing to infer.

### Competing Hypotheses
For each: the hypothesis in one sentence, evidence for, evidence against or missing. Skip if one answer is clear.

### What We Don't Know
Specific unanswered questions, searches that returned nothing (with the queries), unavailable sources and why, and people who would likely know. Historical investigations almost always have gaps; an empty section is suspicious.

### Sources Consulted
One line per category, every category:
- **Source control history**: files, number of commits reviewed, PRs, code comments searched.
- **Issue / ticket tracker**: ticket IDs and queries, or "Not searched. No matching MCP server configured."
- **Long-form documents**: pages and queries, or the reason it was not searched.
- **Real-time team chat**: channels, date ranges, queries, or the reason.
- **Infrastructure observability**: dashboards, monitors, metrics, logs, incidents, or the reason.
- **Error / exception tracking**: issues, events, releases, or the reason.
- **Product analytics warehouse**: tables, time windows and the numeric summaries that mattered, or the reason.

### Confidence Summary
One or two sentences on overall confidence: what is well supported, what is inferred, what could not be answered.

## Check before returning

1. Every claim in What We Found has a citation, or it moves to Inferred or Hypotheses.
2. Phrasing matches the tier: Direct claims may say "because"; Inferred claims may not.
3. Contradictions are surfaced, not quietly resolved.
4. What We Don't Know names specific gaps.
5. An embedded hypothesis was checked, not rubber-stamped.
6. No code is cited as evidence of its own intent.
7. The tone matches the evidence. A confident answer on weak evidence is the failure this exists to prevent.

The value of this answer is its honesty, not its authority. A reader who takes it to the original author or a lead should know exactly which follow-up questions to ask.
