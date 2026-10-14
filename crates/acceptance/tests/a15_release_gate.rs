//! A15: baseline versus candidate is recorded; required regressions block. Every case of
//! packages/coding-agent/test/ultron-release-gate.test.ts with its literal expectations, through the session's
//! host-request path: `kernel_apis::KernelApis` with `services::ReleaseGateModule` (the request handler a cell's
//! `gates.*` calls reach), its clock fixed at 5000 as the TS `now: () => 5_000`. Module replay against
//! release-gate.ts: `crates/services/tests/gates_fixture.rs`; through runtime.py: `kernel_fixture.rs`.

use kernel::{HostHandler, Signal};
use kernel_apis::{DocumentStore, KernelApis, MemoryDocumentStore};
use serde_json::{Map, Value, json};
use services::ReleaseGateModule;
use std::sync::Arc;

const FIXTURE: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
const OTHER_FIXTURE: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

struct Host(KernelApis);

impl Host {
    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        let Value::Object(payload) = payload else {
            unreachable!()
        };
        self.0.handle(kind, payload, &Signal::new())
    }
}

fn setup(store: Arc<MemoryDocumentStore>) -> Host {
    let unwired: Arc<dyn HostHandler> = Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| {
        Err::<Value, String>(format!("Ultron RLM host request is not wired: {kind}"))
    });
    let module = ReleaseGateModule::new(store, Some(Arc::new(|| 5_000)));
    Host(KernelApis::new(unwired).with(Arc::new(module)))
}

fn gate() -> Value {
    json!({
        "id": "release-core",
        "checks": [
            { "name": "unit", "required": true },
            { "name": "typecheck", "required": true },
            { "name": "perf", "required": false },
        ],
        "fixture_hash": FIXTURE,
    })
}

fn run(variant: &str, results: Value) -> Value {
    json!({ "variant": variant, "fixture_hash": FIXTURE, "results": results })
}

fn all_pass() -> Value {
    json!({ "unit": "passed", "typecheck": "passed", "perf": "passed" })
}

fn with(mut base: Value, key: &str, value: Value) -> Value {
    base[key] = value;
    base
}

fn defined(store: Arc<MemoryDocumentStore>) -> Host {
    let host = setup(store);
    host.call("gates.define", gate()).unwrap();
    host
}

fn compare(host: &Host, baseline: Value, candidate: Value) -> Result<Value, String> {
    host.call(
        "gates.compare",
        json!({ "gate_id": "release-core", "baseline": baseline, "candidate": candidate }),
    )
}

fn rejects(outcome: Result<Value, String>, text: &str) {
    match outcome {
        Err(error) => assert!(error.contains(text), "{error:?} lacks {text:?}"),
        Ok(value) => panic!("expected an error containing {text:?}, got {value}"),
    }
}

fn reasons(attempt: &Value) -> String {
    attempt["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn acceptance_a15_passes_when_every_required_check_passes_in_the_candidate() {
    let host = defined(Arc::default());
    let attempt = compare(
        &host,
        run(
            "main@abc",
            json!({ "unit": "passed", "typecheck": "failed", "perf": "passed" }),
        ),
        run("branch@def", all_pass()),
    )
    .unwrap();
    assert_eq!(attempt["decision"], "passed");
    assert_eq!(attempt["regressions"], json!([]));
    assert_eq!(
        attempt["improvements"],
        json!([{ "check": "typecheck", "required": true, "baseline": "failed", "candidate": "passed" }])
    );
}

#[test]
fn acceptance_a15_a_required_regression_blocks() {
    let host = defined(Arc::default());
    let attempt = compare(
        &host,
        run("main", all_pass()),
        run("branch", with(all_pass(), "unit", json!("failed"))),
    )
    .unwrap();
    assert_eq!(attempt["decision"], "blocked");
    assert_eq!(
        attempt["regressions"],
        json!([{ "check": "unit", "required": true, "baseline": "passed", "candidate": "failed" }])
    );
    assert!(reasons(&attempt).contains("Required check unit regressed"));
}

#[test]
fn acceptance_a15_an_optional_regression_is_reported_but_does_not_block() {
    let host = defined(Arc::default());
    let attempt = compare(
        &host,
        run("main", all_pass()),
        run("branch", with(all_pass(), "perf", json!("failed"))),
    )
    .unwrap();
    assert_eq!(attempt["decision"], "passed");
    assert_eq!(
        attempt["regressions"],
        json!([{ "check": "perf", "required": false, "baseline": "passed", "candidate": "failed" }])
    );
    assert!(reasons(&attempt).contains("not blocking"));
}

#[test]
fn acceptance_a15_a_required_check_missing_or_failed_in_the_candidate_blocks_even_without_a_baseline_pass() {
    let host = defined(Arc::default());
    let omitted = compare(
        &host,
        run("main", json!({ "unit": "failed", "typecheck": "missing" })),
        run("branch", json!({ "unit": "passed" })),
    )
    .unwrap();
    assert_eq!(omitted["decision"], "blocked");
    assert_eq!(
        omitted["candidate"]["results"],
        json!({ "unit": "passed", "typecheck": "missing", "perf": "missing" })
    );
    assert_eq!(
        omitted["reasons"],
        json!(["Required check typecheck is missing in candidate"])
    );
    let explicit = compare(
        &host,
        run("main", all_pass()),
        run("branch", with(all_pass(), "typecheck", json!("missing"))),
    )
    .unwrap();
    assert_eq!(explicit["decision"], "blocked");
}

#[test]
fn acceptance_a15_a_gate_definition_is_frozen_once_defined() {
    let host = defined(Arc::default());
    // Identical content, in any check order, is idempotent.
    let mut reversed = gate();
    reversed["checks"].as_array_mut().unwrap().reverse();
    let again = host.call("gates.define", reversed).unwrap();
    let original = host
        .call("gates.history", json!({ "gate_id": "release-core" }))
        .unwrap();
    assert_eq!(again["digest"], original["gate"]["digest"]);
    let mut loosened = gate();
    loosened["checks"][0]["required"] = json!(false);
    rejects(host.call("gates.define", loosened), "cannot be changed");
    rejects(
        host.call("gates.define", with(gate(), "fixture_hash", json!(OTHER_FIXTURE))),
        "cannot be changed",
    );
    // A run cannot introduce checks the gate does not define.
    rejects(
        compare(
            &host,
            run("main", all_pass()),
            run("branch", with(all_pass(), "bonus", json!("passed"))),
        ),
        "does not define: bonus",
    );
    rejects(
        host.call(
            "gates.define",
            with(
                with(gate(), "id", json!("optional-only")),
                "checks",
                json!([{ "name": "a", "required": false }]),
            ),
        ),
        "at least one required check",
    );
}

#[test]
fn acceptance_a15_a_fixture_mismatch_blocks_with_a_reason() {
    let host = defined(Arc::default());
    let attempt = compare(
        &host,
        run("main", all_pass()),
        with(run("branch", all_pass()), "fixture_hash", json!(OTHER_FIXTURE)),
    )
    .unwrap();
    assert_eq!(attempt["decision"], "blocked");
    let reasons = attempt["reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 1);
    assert!(
        reasons[0]
            .as_str()
            .unwrap()
            .contains("Fixture mismatch: candidate branch")
    );
}

#[test]
fn acceptance_a15_every_attempt_blocked_or_passed_is_recorded_in_history() {
    let host = defined(Arc::default());
    let blocked = compare(
        &host,
        run("main", all_pass()),
        run("try-1", with(all_pass(), "unit", json!("failed"))),
    )
    .unwrap();
    let passed = compare(&host, run("main", all_pass()), run("try-2", all_pass())).unwrap();
    let history = host
        .call("gates.history", json!({ "gate_id": "release-core" }))
        .unwrap();
    let attempts: Vec<Value> = history["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| json!([a["id"], a["decision"]]))
        .collect();
    assert_eq!(
        attempts,
        vec![json!([blocked["id"], "blocked"]), json!([passed["id"], "passed"])]
    );
    assert!(
        history["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["gate_digest"] == history["gate"]["digest"])
    );
}

#[test]
fn acceptance_a15_rejects_unknown_fields_and_unknown_gates() {
    let host = defined(Arc::default());
    rejects(
        host.call(
            "gates.define",
            with(with(gate(), "id", json!("g2")), "strict", json!(true)),
        ),
        "Unknown payload field: strict",
    );
    rejects(
        host.call(
            "gates.define",
            with(
                with(gate(), "id", json!("g3")),
                "checks",
                json!([{ "name": "unit", "required": true, "weight": 2 }]),
            ),
        ),
        "Unknown payload field: weight",
    );
    rejects(
        host.call(
            "gates.compare",
            json!({ "gate_id": "release-core", "baseline": run("main", all_pass()),
                    "candidate": run("branch", all_pass()), "override": "passed" }),
        ),
        "Unknown payload field: override",
    );
    rejects(
        compare(
            &host,
            with(run("main", all_pass()), "notes", json!("x")),
            run("branch", all_pass()),
        ),
        "Unknown field baseline.notes",
    );
    rejects(
        compare(
            &host,
            run("main", all_pass()),
            run("branch", with(all_pass(), "unit", json!("skipped"))),
        ),
        "passed, failed, or missing",
    );
    rejects(
        host.call("gates.history", json!({ "gate_id": "nope" })),
        "Unknown release gate",
    );
}

#[test]
fn acceptance_a15_gates_and_attempts_survive_a_new_module_instance_and_tampered_definitions_are_refused() {
    let store: Arc<MemoryDocumentStore> = Arc::default();
    let first = defined(store.clone());
    let attempt = compare(&first, run("main", all_pass()), run("branch", all_pass())).unwrap();
    let second = setup(store.clone());
    let history = second
        .call("gates.history", json!({ "gate_id": "release-core" }))
        .unwrap();
    let ids: Vec<&Value> = history["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["id"])
        .collect();
    assert_eq!(ids, vec![&attempt["id"]]);
    rejects(
        second.call("gates.define", with(gate(), "fixture_hash", json!(OTHER_FIXTURE))),
        "cannot be changed",
    );

    let mut saved = store.read().unwrap().unwrap();
    for check in saved["gates"][0]["checks"].as_array_mut().unwrap() {
        check["required"] = json!(false);
    }
    store.write(&saved).unwrap();
    let third = setup(store);
    rejects(third.call("gates.list", json!({})), "does not match its digest");
}
