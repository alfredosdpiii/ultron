# services

Six independent Python services (standard library only, Python 3.10+). They share no code.

| Service | Package | What it does |
| --- | --- | --- |
| services/kvstore | segstore | log-structured key-value store with compaction and recovery |
| services/scheduler | jobqueue | discrete-event job scheduler with retries, cancellation and dependencies |
| services/patch | linediff | line diffs, unified hunks and patch application |
| services/calendar | slots | meeting-slot finding over working hours and busy blocks |
| services/wire | frames | binary codec and framed stream decoder |
| services/builds | depgraph | build dependency graph, incremental rebuilds and version resolution |

Each service's behaviour is specified in its SPEC.md. Each service has a check harness, run from the service's
directory:

    cd services/kvstore && python3 harness.py

The harness is slow (a full run takes a little over 20 seconds). It runs its checks in five stages and stops at
the first stage that fails. Every run is logged to the service's `.harness/runs.jsonl`.
