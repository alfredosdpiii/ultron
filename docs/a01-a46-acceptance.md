# Ultron A01-A46 acceptance

The acceptance matrix comes from the plan's section 13. Row status is not written by hand: it is computed by the acceptance runner from `acceptance/manifest.json`, and the latest result is in [acceptance/report.md](../acceptance/report.md).

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
