# DeepSWE swarm A/B: 4 tasks, ultron vs ultron-swarm

Date 2026-10-20. Model `cliproxyapi/gpt-6.1-sol` through CLIProxyAPI. Binary `ultron-cf87c0643-portable` (0.87.39, commit cf87c0643).
Dataset `datacurve/deep-swe` (Harbor format); one fresh container per (task, arm) from the task image, graded in a second fresh container by the task's own verifier.

| task | language | arm | verifier | status | wall | responses (children) | tokens (root/children) | notional cost | children | swarm calls (host) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| arcane-drift-detection-baselines | go | ultron | fail | completed | 22.0 min | 112 (55) | 3,476k (1,974k/1,502k) | $0.99 | 2 | rlm.spawn 2, swarm.agents 3, swarm.inbox 12, swarm.send 15, rlm.collect 5, rlm.ask 1 |
| arcane-drift-detection-baselines | go | ultron-swarm | fail | completed | 31.0 min | 204 (186) | 5,464k (400k/5,064k) | $1.65 | 10 | board.add 7, rlm.spawn 10, board.claim 9, board.list 4, swarm.route 7, swarm.send 41, swarm.agents 3, swarm.inbox 22, rlm.collect 8, board.update 10, rlm.ask 5 |
| kysely-window-grouping-helpers | typescript | ultron | pass | completed | 13.1 min | 104 (53) | 2,318k (1,137k/1,181k) | $0.72 | 2 | rlm.spawn 2, swarm.inbox 6, swarm.send 4, swarm.agents 1, rlm.collect 4, rlm.ask 1 |
| kysely-window-grouping-helpers | typescript | ultron-swarm | pass | completed | 26.2 min | 285 (268) | 4,392k (275k/4,117k) | $1.92 | 23 | board.add 8, rlm.spawn 23, board.claim 8, board.list 3, rlm.collect 22, swarm.agents 2, swarm.send 21, board.update 10, rlm.ask 8, swarm.route 5, swarm.inbox 1 |
| wasmi-trap-coredumps | rust | ultron | pass | completed | 9.1 min | 85 (38) | 3,181k (2,245k/936k) | $0.87 | 2 | rlm.spawn 2, rlm.ask 1, rlm.collect 3 |
| wasmi-trap-coredumps | rust | ultron-swarm | pass | completed | 45.0 min | 454 (422) | 11,509k (990k/10,519k) | $4.46 | 29 | board.add 5, rlm.spawn 29, rlm.ask 28, board.claim 5, board.list 5, swarm.agents 2, swarm.send 45, rlm.collect 20, swarm.route 5, board.update 5, swarm.inbox 16 |
| sqlfmt-create-table-ddl-formatting | python | ultron | fail | completed | 8.0 min | 72 (23) | 2,239k (1,690k/549k) | $0.62 | 1 | rlm.spawn 1, rlm.ask 1, swarm.send 8, swarm.inbox 5, rlm.collect 2 |
| sqlfmt-create-table-ddl-formatting | python | ultron-swarm | pass | timeout | 90.1 min | 219 (195) | 3,690k (513k/3,177k) | $1.64 | 15 | board.add 4, rlm.spawn 15, board.list 4, board.claim 4, swarm.send 14, rlm.collect 15, swarm.inbox 8, rlm.ask 14, board.update 6, swarm.route 4 |

| arm | passed | timeouts | harness failures | median wall | total wall | responses | tokens | notional cost | children | swarm calls |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ultron | 2/4 | 0 | 0 | 11.1 min | 52.1 min | 373 | 11,214k | $3.21 | 7 | 79 |
| ultron-swarm | 3/4 | 1 | 0 | 38.0 min | 192.3 min | 1162 | 25,056k | $9.67 | 77 | 490 |

Gold check (reference patches through the same grading): arcane-drift-detection-baselines pass, kysely-window-grouping-helpers pass, wasmi-trap-coredumps pass, sqlfmt-create-table-ddl-formatting pass.

- Tasks: narwhals-rolling-window-suite (the brief's Python task) was swapped for sqlfmt-create-table-ddl-formatting: its gold patch fails in its image (test.sh base: 6 failures unrelated to the task, polars DeprecationWarnings in the interchange tests and a get_column assertion). Gold passes for all four tasks run.
- Agent containers ran with --network host (the proxy is on 127.0.0.1:8317), departing from the tasks' allow_internet=false; grading containers ran with --network none.
- One trial per (task, arm): a 4-task, single-trial A/B; differences of one task are within run-to-run noise.
- Tree usage (responses, tokens, cost) is the host's budget ledger in the session journal: ultron usage --json in 0.87.39 reports the root lane only and shows zero sub-agent usage even when children ran. Children's tokens have no cached/uncached split; cost is the ledger's, at the models.json list prices (notional: the proxy bills a subscription).
- Swarm calls are the host's counters (hostCalls), children's calls included; children = the host's rlm.spawn count.
- sqlfmt ultron-swarm hit the 90-minute limit: from minute 20 the root sat in one `await rlm.ask(<kept child>, ...)` with no journal write for 67 minutes until the limit; its working tree at the limit passed the verifier.
- The sqlfmt pair ran in a second harness process (run id ab4b) while the first finished wasmi; its records were merged into this run unchanged.
