### Runtime forensics

**You own the diagnosis. Instrument the live process, don't theorize from source.** The deliverable is a cited
diagnosis, not a fix.

1. Capture the live signal on the matching surface: a CPU profile for a spinning process, a heap snapshot for a leak,
   a DevTools trace for a visual glitch (through a browser MCP server, or the runtime's own flags such as
   `node --cpu-prof`, `py-spy`, `perf`). A real artifact, not a guess.
2. Reduce it to the smoking gun: the hot-path function, the retainer chain from the leaked object to a GC root, the
   loop firing without input. Parse it in the REPL (`h = await rlm.load(path=...)`, `json`, sqlite) and print only
   the reduced finding (`the guard-the-context-window principle (`../principle-guard-the-context-window/SKILL.md`)`).
3. Prove the mechanism before believing it. Inject instrumentation into the running process (a debugger or DevTools
   eval, a log line in the dev build) to confirm the hypothesis cheaply.
4. Map the finding to source: file, symbol, the line that allocates or schedules.
5. Throughput checkpoint stays one line: `throughput checkpoint: n/a, read-only forensics`.

**Reply:** the signal captured, the reduced finding, how you proved the mechanism, the source location, artifact
paths. No fix unless asked; hand back to Bug fix or Perf issue once the cause is known.
