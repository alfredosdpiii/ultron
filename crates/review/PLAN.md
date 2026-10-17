# crates/review: autoreview in Rust

RLM-first: the model's only tool is the Python REPL. `/review` is one kernel cell (`await review_api.run(rlm, args)`)
and an autoreview is an engine run of `autoreview_api.run` in a kernel of its own (decision row 257): the review
pipeline (scope, chunks, findings, deep, reference, compiled and test passes) is the kernel Python, unchanged. The
host serves its `rlm.*` inference and owns the GitHub side.

## What is Rust

| Source | Rust | Status |
| --- | --- | --- |
| `autoreview/lessons.ts` ledger | `fatesOf`, `mergeFates`, `lessonsFor` | ported: `ledger.rs` |
| `autoreview/config.ts` settings | `resolveConfig`, `engineSettings`, `testsEligible`, `autoreviewPaths`, `resolveAckArt` | ported: `settings.rs` |
| `autoreview/prepare.ts` | `detectEcosystems`, `toolchainRequests`, `environmentHash`, `findMise`, `describeToolchain`; `prepareEnvironment`/`cachedEnvironment` | ported: `prepare.rs`, `prepare_env.rs` |
| `autoreview/plan.ts` + `types.ts` levels | `planReview`, `decideVerdict`, `rankFindings`, `placeLine`, `planComment`, `commentText`, `costLine`, `levelOf`, `severityOf` | ported: `plan.rs` |
| `autoreview/*.ts` rest | engine (`engine.rs`: `autoreview_api.run_file` in a kernel of its own, transient provider failures retried), runner, accounts, GitHub client, checkout, local, state, reviewer (lessons via `LessonMemory`, implemented by crates/hindsight), daemon, service files | ported; the command is crates/cli `autoreview_cli.rs` |

`ultron autoreview doctor` runs `autoreview_tests.py doctor` / `environments` with the kernel's interpreter, as
cli.ts does: the sandbox it reports is the one reviews use.

A Rust port of the review Python (`review_api.py`, `autoreview_api.py`, `autoreview_deep.py`,
`autoreview_reference.py`, `autoreview_compiled.py`, `autoreview_tests.py`) was removed on 2026-10-17: nothing in
the product called it, since reviews run the Python.

## Evidence

`fixtures/generate.mjs` imports `autoreview/lessons.ts` (`ledger.json`), `config.ts` (`settings.json`), `prepare.ts`
over repository trees it builds in a temp dir (`prepare.json`; temp paths written as `<root>`) and `plan.ts`
(`plan.json`), then chains `generate_github.mjs`, `generate_daemon.mjs` and `generate_service.mjs`;
`generate_prepare_env.mjs` records `prepare_env.json`. `tests/ledger.rs`, `tests/autoreview.rs` and the area tests
replay them exactly; crates/cli/fixtures/autoreview_offline.mjs records the TypeScript
`ultron autoreview review --repo-dir --json` against a local stub provider (`crates/cli/tests/autoreview.rs`).
Regenerate with `node crates/review/fixtures/generate.mjs` (deterministic; sha256 stable across runs).

JavaScript semantics the TypeScript ports rely on live in `js.rs`: `\s`/`trim`, UTF-16 lengths and slices,
`toFixed`, `Math.round`, and `path.join`.

Known differences, all outside recorded behaviour: `requirements/` being a file makes `detectEcosystems` throw in TS
and list nothing here; `ackArt: true` throws in TS and is the logo here.
