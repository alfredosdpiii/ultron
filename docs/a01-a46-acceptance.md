# Ultron acceptance (A01-A56)

The acceptance matrix comes from the plan's section 13. Row status is not written by hand: it is computed by the acceptance runner from `acceptance/manifest.json`, and the latest result is in [acceptance/report.md](../acceptance/report.md).

## Rows added by the supreme plan

A01-A46 are the original acceptance rows. The supreme plan (`supremeplan.md`) added:

| Row | Phase | Behavior |
|---|---|---|
| A47 | 2 | An input larger than the model's window is processed to the exact answer; no request exceeds the window and the root transcript never contains the input |
| A48 | 2 | Contract repair: a malformed frame answer is re-asked; exhaustion returns `Incomplete` with evidence and the root turn continues |
| A49 | 2 | Budget subtree: a map under a call budget runs what fits and marks the rest incomplete; tokens never exceed the pool; failed requests refund |
| A50 | 2 | Aborting the root cancels every running frame within 2 s |
| A51 | 3 | After many completed tasks root requests stay bounded (collapse on return) while `agents.result` returns full values |
| A52 | 3 | `ctx.forget` never removes the current user turn or pinned items; extensions observe edits; notes and pins survive compaction |
| A53 | 4 | A code skill whose test fails is never importable; a passing one is, survives restart, and rolls back cleanly |
| A54 | 4 | A repeated task reuses a saved code skill and costs less the second time |
| A55 | 6 | An agent class defined in a cell is invokable typed end to end, keeps state across cells and snapshots, and rejects a bad return with the schema error |

The asynchronous-execution work (Unreal Agent's model) added:

| Row | Behavior |
|---|---|
| A56 | Long-running work does not block the turn and completion is delivered without polling: `bash(cmd, yield_after=)` jobs and slow extension/MCP tool calls from the REPL run in the host and survive the cell and kernel restarts; a completion is appended as a `<runtime_event>` that re-invokes an idle root once per batch or reaches a running turn at its next boundary, within the turn budget, never after an abort; root requests keep a byte-identical prefix |

## How a row is judged

- `passed`: every listed evidence test passed, the reviewed manifest declares the row passable, the instrument lock matches, and no mutation of the row's guarantee survived.
- `failed`: any listed evidence test failed.
- `unverified`: evidence is missing or skipped, the runner crashed or timed out (an infrastructure outcome is never a pass), the instrument changed without review, or a mutation survived.
- Metered live rows (A16, A39, A46) skip their live tests by default and pass only on a recorded live run in `acceptance/capabilities/` or `acceptance/demonstrations/` that met the row's bar. Missing evidence is never excused by a live record.
- A26 (the instrument itself) passes when the lock is intact and the mutation slice kills every mutation.

## Commands

```bash
npm run test:acceptance            # run evidence, write acceptance/report.{json,md}
npm run test:acceptance:mutation   # regression-mutation slice -> acceptance/mutation.json
npm run acceptance:lock            # re-lock after a reviewed change to the manifest or evidence

# Metered, opt-in live runs (from packages/coding-agent)
ULTRON_LIVE_EVAL=1 ULTRON_LIVE_EVAL_MODELS=cliproxyapi/gpt-6-sol npx vitest --run test/ultron-a39-live-capabilities.test.ts
ULTRON_LIVE_EVAL=1 ULTRON_LIVE_EVAL_MODEL=cliproxyapi/gpt-6-sol npx vitest --run test/ultron-a46-live-demo.test.ts
ULTRON_LIVE_HINDSIGHT=1 npx vitest --run test/ultron-a16-live-hindsight.test.ts
```

Live runs copy only `models.json` and `auth.json` from the profile into an isolated agent directory. Provider errors and runner crashes are recorded as infrastructure outcomes, not model failures.

## Reused Pi components

Every agent run uses the native RLM worker. `--help` and `--list-models` use Pi's handlers because they never run the agent; HTML export uses Pi's renderer on converted native entries; extensions, skills, and prompt templates are loaded by Pi's resource loader inside the worker; themes use Pi's loader in the native TUI.
