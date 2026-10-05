### Trace forensics

**You own the diagnosis from the artifact. Load it, shape it, narrow to the cause, attribute to source.**

Distinct from Runtime forensics, which instruments a live process. Here the capture already exists: read it, don't
re-run it.

1. Identify the format (cpuprofile, `.json.gz` trace, spindump, heapsnapshot) and load it in the REPL. Keep the raw
   data in variables; print only reductions (`the guard-the-context-window principle (`../principle-guard-the-context-window/SKILL.md`)`).
2. Transform it into a queryable form before reading: sqlite, one row per sample, frame or node.
3. Narrow to the cause. Query for the frames holding the most time and walk the call tree to the hot path. For a
   leak, follow the retainer chain to a GC root. For a spindump, find the thread stuck on-CPU or blocked and its wait
   reason.
4. Attribute to source through the artifact's own symbols: file, symbol, line. A frame with no source mapping is not
   yet a diagnosis; resolve the symbols or say the artifact lacks them.
5. Confirm against a paired capture when you have one (diff before and after). Without one, mark the finding as the
   strongest hypothesis the artifact supports.
6. Hand back a cited diagnosis, no fix unless asked. Route to Bug fix or Perf issue once the cause is known.
   Throughput checkpoint: `n/a, read-only forensics`.

**Reply:** the artifact and format, the reduced finding, the source location, artifact paths, and whether a paired
capture confirmed it.
