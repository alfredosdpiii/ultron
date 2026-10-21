# Autoreview graph queries: first live use (redacted)

Date: 2026-10-22. The investigators' `graph` request (one read-only SQL query over the whole repository's code
graph at the reviewed commit, as in CodexGraph, arXiv 2408.03910) on two private pull requests of the same
repository. Both reviews were offline (`ultron autoreview review --repo-dir`), deep pass only, no test runs, with
`cliproxyapi/gpt-6.1-sol` for every frame. Each ran with private server, agent and knowledge directories. Offline
replays have no PR title, description, lessons or other reviewers' comments, so they say whether the graph is
used, not how much it improves reviews.

The graph at the reviewed commit:

| Files | Symbols | Calls | Imports | Constructs | Store | First index | Same commit again |
|---|---|---|---|---|---|---|---|
| 3,486 | 33,742 | 328,225 | 37,216 | 100,586 | 84 MB | 30 s | 0.14 s |

Query times on it: the callers of one helper, 4 ms (357 calls in 74 files); the most-imported files, 0.45 s; the
controllers without a guard decorator, 28 ms; a join of every call to every method by name, 3.6 s (the cap is
5 s); the same join narrowed to one file, 2 ms.

| PR | Change | Investigators | Lookups | Graph queries | Rejected | Findings | Cost |
|---|---|---|---|---|---|---|---|
| F | 2 files, +375 -6: one module and its tests | 4 | 49 | 0 | 0 | 3 | $0.89 |
| G | 18 files, +2,153 -309: migrations, a worker and its service, CI | 5 | 68 | 3 (siblings) | 0 | 0 | $1.15 |

Findings:

- The request is offered and works live: every graph query the model wrote ran, and none was rejected.
- The model reaches for it when a question spans the repository. In G it was the siblings investigator, whose part
  is the other producers and consumers of what the change touches. In F everything it needed was in the two files
  and their callers, which the bounded lookups already serve.
- Whether it improves reviews is not measured here. That needs the blind set with the live context.
- The record now keeps the first five query texts per investigator (`queries`), so live reviews show what was asked.
