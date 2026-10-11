# crates/review: /review and autoreview in Rust

RLM-first: the model's only tool is the Python REPL. `/review` stays one cell (`await review_api.run(rlm, args)`)
and autoreview stays an engine run of `autoreview_api.run`; both become thin Python over Rust host operations.
The host owns everything deterministic, every process, and every guarantee (budgets, deadlines, permissions,
secret masking, Loki). Python keeps only what is model-facing: orchestrating `rlm.map` / `rlm.infer` frames and
the planner's sandboxed `rv` cells.

## What becomes Rust

| Source | Rust (host operation) | Status |
| --- | --- | --- |
| `review_prompts.py` | prompt texts and builders (`finder_task`, `autoreview_finder_task`, `deep_task`, planner tasks, catalogue) | ported: `prompt_text.rs` (verbatim), `prompts.rs` |
| `review_api.py` arguments | `parse_args`, `classify_targets` (shlex, env defaults, errors) | ported: `args.rs`, `py.rs` |
| `review_api.py` diff | `parse_diff`, `added_file`, `skip_reason`, `file_kind`, `render_hunk` | ported: `diff.rs` |
| `review_api.py` findings | `normalize_findings`, `normalize_category`, `similar_claims`, `dedupe`, `evidence_quotes_source`, `parse_json_object`, `apply_verdicts` | ported: `findings.rs` |
| `autoreview/lessons.ts` ledger | `fatesOf`, `mergeFates`, `lessonsFor` | ported: `ledger.rs` |
| `autoreview/config.ts` settings | `resolveConfig` (defaults, clamps, model fallback chain, per-model thinking), `engineSettings`, `testsEligible`, `autoreviewPaths`, `resolveAckArt` (the logo text is passed in: the TUI owns the art) | ported: `settings.rs` |
| `autoreview/prepare.ts` discovery | `detectEcosystems` (bounded nested walk), `toolchainRequests`, `environmentHash`, `findMise`, `describeToolchain` | ported: `prepare.rs`; building (`uv`, `npm ci`, `mise install`) is next, as host jobs |
| `autoreview/plan.ts` + `types.ts` levels | `planReview`, `decideVerdict`, `rankFindings`, `placeLine`, `planComment`, `commentText`, `costLine`, `withoutInline`, `levelOf`, `severityOf` | ported: `plan.rs` |
| `autoreview_api.py` rubric and retries | `to_level`, `set_level`, `_holds`, `capped_level`, `final_level`, `finding_kind`, `generic_reason` (+ `is_code`), `apply_lessons`, `is_transient`, `is_auth_failure`, `is_throttled`, `retry_after` | ported: `levels.rs` (findings stay JSON objects, updated in place as the Python dicts were) |
| `review_api.py` rest | chunking (`build_chunks`, `plan_find`, `applies`), scope (git/gh diff resolution, readers, untracked), `related_code`, `render_report`, `_save`, `post` | next |
| `autoreview_api.py` rest | context/intent/guides, line mapping (`map_line`, `diff_line_ranges`), `pack_chunks`, `merged_reviewers`, `dedupe_fast`, `group_root_causes`, `_public`, `parse_rules`, the `Frames` scheduler (concurrency per model, deadlines, retries with the classification above) | next; `Frames` becomes the host's frame scheduler on durable tasks |
| `autoreview_deep.py` | request serving (`serve_request`, `serve_blocks`), citation checks, `merge`, shapes, regression findings | next |
| `autoreview_reference.py` | structural index (Python/TS/YAML/Terraform facts) | next; pure, fixture-friendly |
| `autoreview_compiled.py` | program validation (`validate`, `grounded`, coverage), step execution, template steps | next; the `rv` API calls become host operations |
| `autoreview_tests.py` | sandbox detection, runners, outcome parsing | next; process supervision is the host's (`host` crate jobs) |
| `autoreview/*.ts` rest | state store (`state.ts`), `prepareEnvironment`/`resolveToolchain`/`cachedEnvironment`, accounts, GitHub client, checkout manager, reviewer loop, daemon, service files, CLI | next; daemon/CLI land in `cli`/`server` crates, this crate keeps the pure parts |

## What stays Python (thin API)

- `review_api.run(rlm, args)`: calls host ops for args/scope/chunks, runs finder and verifier frames with
  `rlm.map`, hands replies back to host ops (`normalize_findings`, `dedupe`, `apply_verdicts`), prints the report.
- `review_api.post(review, confirm=True)`: forwards to the host, which enforces the confirmation.
- `autoreview_api.run(rlm, spec)` and the deep/compiled passes: the frame orchestration only; every lookup, test
  run, and validation is a host operation, and budgets/deadlines are enforced by the host, not by Python.
- The planner's `rv` object in compiled cell mode: a Python facade whose every call is a validated host op.

## Evidence

`fixtures/generate.mjs` runs `fixtures/record.py` against the Python modules in this checkout (`review.json`,
including the `autoreview_api.py` rubric under `autoreview`) and imports `autoreview/lessons.ts` (`ledger.json`),
`config.ts` (`settings.json`), `prepare.ts` over repository trees it builds in a temp dir (`prepare.json`; the
trees are recorded and rebuilt by the Rust test, temp paths written as `<root>`) and `plan.ts` (`plan.json`).
`tests/fixtures.rs`, `tests/ledger.rs` and `tests/autoreview.rs` replay them exactly. Regenerate with
`node crates/review/fixtures/generate.mjs` (deterministic; sha256 stable across runs).

JavaScript semantics the TypeScript ports rely on live in `js.rs`: `\s`/`trim` (U+FEFF yes, U+0085 no), UTF-16
lengths and slices, `toFixed` (ties round up, unlike Rust's formatting), `Math.round`, and `path.join`.

Known differences, all outside recorded behaviour: a UTF-16 cut inside a surrogate pair keeps the pair out (JS
would leave a lone surrogate); `requirements/` being a file makes `detectEcosystems` throw in TS and list nothing
here; `ackArt: true` throws in TS and is the logo here.
