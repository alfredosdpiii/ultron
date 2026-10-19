//! A14: no checks, an incomplete review, or a stale receipt can imply verified completion. Every case of
//! packages/coding-agent/test/ultron-a14-no-false-verification.test.ts with its literal expectations, over the TS
//! `verificationFixture` rebuilt on the Rust stack (`services_support`): the typed-agent host with its built-in
//! reviewer definitions, the deterministic check adapter (echoes `input.verdict`, crashes on `input.fail`) and an
//! echoing predict adapter, reviewer lanes answering with the JSON text of their request, and the progress,
//! schedules-and-goals and release-gate modules behind the same request path as a cell's.

mod agents_support;
mod services_support;

use agents_support::{Memory, aborted, adapter, definition, input_of};
use delegation::task_store::TaskStore;
use kernel_apis::{MemoryDocumentStore, ProgressModule, SchedulesModule};
use serde_json::{Value, json};
use services::ReleaseGateModule;
use services_support::{Host, Setup, build, rejects};
use std::sync::Arc;

const FIXTURE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FIXTURE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn verification_host(store: Memory) -> Host {
    let host = build(Setup {
        tasks: Box::new(store),
        // Reviewer lanes answer with the JSON text given in their request.
        script: Some(Arc::new(|_, prompt, signal| {
            let request = input_of(prompt)["request"].as_str().unwrap_or_default().to_string();
            if request == "hang" {
                return aborted(signal);
            }
            Ok(request)
        })),
        // Checks and verifiers echo `input.verdict`, or crash on `input.fail`.
        deterministic: Some(adapter(|_, input, _, _| {
            if input
                .get("fail")
                .is_some_and(|f| f != &Value::Null && f != &json!(false))
            {
                return Err("check crashed".into());
            }
            Ok(input.get("verdict").cloned().unwrap_or_else(|| input.clone()))
        })),
        predict: Some(adapter(|_, input, _, _| Ok(input.clone()))),
        modules: vec![
            Arc::new(ProgressModule::new(Arc::new(MemoryDocumentStore::default()))),
            Arc::new(SchedulesModule::new(Arc::new(MemoryDocumentStore::default()), None)),
            Arc::new(ReleaseGateModule::new(Arc::new(MemoryDocumentStore::default()), None)),
        ],
        ..Setup::default()
    });
    let any = json!({ "inputSchema": {}, "outputSchema": {} });
    host.call(
        "agents.register",
        json!({ "definition": definition("check", "deterministic", any.clone()) }),
    )
    .unwrap();
    host.call(
        "agents.register",
        json!({ "definition": definition("guess", "predict", any) }),
    )
    .unwrap();
    host
}

fn review(request: &str) -> Value {
    json!({ "definition": "security-reviewer@1", "input": { "request": request } })
}

fn with(mut base: Value, key: &str, value: Value) -> Value {
    base[key] = value;
    base
}

const NO_FINDINGS: &str = r#"{"outcome":"no_findings","findings":[]}"#;

#[test]
fn acceptance_a14_every_task_result_path_is_unverified_and_a_stored_verified_result_is_refused() {
    let store = Memory::default();
    let host = verification_host(store.clone());
    let invoke = |payload: Value| host.call("agents.invoke", payload).unwrap();
    let results = [
        invoke(json!({ "definition": "check@1", "input": { "verdict": { "passed": true } } })),
        invoke(json!({ "definition": "check@1", "input": { "fail": true } })),
        invoke(json!({ "definition": "guess@1", "input": { "passed": true } })),
        invoke(review(NO_FINDINGS)),
        invoke(review("not json")),
        invoke(with(review("hang"), "timeout_ms", json!(5))),
        invoke(with(review(NO_FINDINGS), "key", json!("k"))),
        invoke(with(review(NO_FINDINGS), "key", json!("k"))),
    ];
    let statuses: Vec<&str> = results.iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(
        statuses,
        [
            "succeeded",
            "failed",
            "succeeded",
            "succeeded",
            "failed",
            "cancelled",
            "succeeded",
            "succeeded"
        ]
    );
    for result in &results {
        assert_eq!(result["verification"], "unverified", "{result}");
    }
    let mut reader = store.clone();
    let document = reader.read().unwrap().unwrap();
    for task in document["tasks"].as_array().unwrap() {
        assert_eq!(task["result"]["verification"], "unverified", "{task}");
    }
    host.close();

    // An owner that ended mid-task recovers the task as interrupted, still unverified.
    let running = json!({
        "id": "ultron-task-crashed", "key": "crashed", "fingerprint": "c".repeat(64),
        "definition": "rlm-child@1", "state": "running",
    });
    let mut crashed = Memory::default();
    crashed.write(&json!({ "version": 1, "tasks": [running] })).unwrap();
    let reopened = build(Setup {
        tasks: Box::new(crashed),
        ..Setup::default()
    });
    assert_eq!(
        reopened
            .call("agents.result", json!({ "id": "ultron-task-crashed" }))
            .unwrap(),
        json!({
            "status": "interrupted",
            "error": "Owner ended; automatic replay is disabled",
            "verification": "unverified",
        })
    );
    reopened.close();
    // A journal claiming verified completion is rejected outright.
    let mut forged = Memory::default();
    let mut completed = running.clone();
    completed["state"] = json!("completed");
    completed["result"] = json!({ "status": "succeeded", "value": "done", "verification": "verified" });
    forged.write(&json!({ "version": 1, "tasks": [completed] })).unwrap();
    let refused = build(Setup {
        tasks: Box::new(forged),
        ..Setup::default()
    });
    rejects(refused.call("agents.tasks", json!({})), "Invalid task document");
}

#[test]
fn acceptance_a14_reviewer_output_missing_required_fields_fails_an_incomplete_review_verifies_nothing() {
    let host = verification_host(Memory::default());
    for malformed in [
        r#"{"outcome":"no_findings"}"#,
        r#"{"findings":[]}"#,
        r#"{"outcome":"findings","findings":[{"file":"a.ts","severity":"high","explanation":"x"}]}"#,
        r#"{"outcome":"clean","findings":[]}"#,
    ] {
        assert_eq!(
            host.call("agents.invoke", review(malformed)).unwrap(),
            json!({
                "status": "failed",
                "error": "security-reviewer@1 output does not match its schema",
                "verification": "unverified",
            }),
            "{malformed}"
        );
    }

    let incomplete = host
        .call("agents.spawn", review(r#"{"outcome":"incomplete","findings":[]}"#))
        .unwrap();
    assert_eq!(
        host.call("agents.result", json!({ "id": incomplete["id"] })).unwrap(),
        json!({
            "status": "succeeded",
            "value": { "outcome": "incomplete", "findings": [] },
            "verification": "unverified",
        })
    );
    // Claiming the incomplete review complete, verified by another reviewer, stays unverified: reviewer output
    // carries no boolean `passed`.
    let decision = host
        .call(
            "progress.reassess",
            json!({
                "task_id": incomplete["id"], "claim": "complete", "verifier": "correctness-reviewer@1",
                "verifier_input": { "request": NO_FINDINGS },
            }),
        )
        .unwrap();
    assert_eq!(decision["claim"]["decision"], "unverified");
    assert_eq!(
        decision["claim"]["reason"],
        "Verifier output has no boolean passed field"
    );
    // A failed review cannot even be claimed complete.
    let failed = host
        .call("agents.spawn", review(r#"{"outcome":"no_findings"}"#))
        .unwrap();
    host.call("agents.result", json!({ "id": failed["id"] })).unwrap();
    let rejected = host
        .call(
            "progress.reassess",
            json!({ "task_id": failed["id"], "claim": "complete" }),
        )
        .unwrap();
    assert_eq!(rejected["claim"]["decision"], "rejected");
    host.close();
}

#[test]
fn acceptance_a14_a_progress_completion_claim_is_verified_only_by_a_host_run_verifier_that_returns_passed_true() {
    let host = verification_host(Memory::default());
    let task = host
        .call(
            "agents.spawn",
            json!({ "definition": "check@1", "input": { "work": 1 } }),
        )
        .unwrap();
    host.call("agents.result", json!({ "id": task["id"] })).unwrap();
    let claim = |extra: Value| -> Value {
        let mut payload = json!({ "task_id": task["id"], "claim": "complete" });
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        host.call("progress.reassess", payload).unwrap()["claim"].clone()
    };
    let none = claim(json!({}));
    assert_eq!(none["decision"], "unverified");
    assert_eq!(
        none["reason"],
        "No verifier was named; a task result alone is not verification"
    );
    for input in [
        json!({ "verdict": { "passed": false } }),
        json!({ "verdict": { "passed": "yes" } }),
        json!({ "fail": true }),
    ] {
        assert_eq!(
            claim(json!({ "verifier": "check@1", "verifier_input": input }))["decision"],
            "unverified"
        );
    }
    assert_eq!(claim(json!({ "verifier": "missing@1" }))["decision"], "unverified");
    // Positive control: the verified path exists and needs an explicit pass.
    assert_eq!(
        claim(json!({ "verifier": "check@1", "verifier_input": { "verdict": { "passed": true } } }))["decision"],
        "verified"
    );
    host.close();
}

#[test]
fn acceptance_a14_a_goal_with_no_required_checks_or_a_check_without_an_explicit_pass_is_never_achieved() {
    let host = verification_host(Memory::default());
    let empty = host
        .call("goals.create", json!({ "title": "no checks", "required_checks": [] }))
        .unwrap();
    let unchecked = host.call("goals.verify", json!({ "id": empty["id"] })).unwrap();
    assert_eq!(unchecked["state"], "active");
    assert_eq!(unchecked["verification"]["status"], "unverified");
    assert_eq!(
        unchecked["verification"]["reasons"],
        json!(["Goal has no required checks"])
    );

    let goal = host
        .call(
            "goals.create",
            json!({ "title": "checked", "required_checks": ["check@1"] }),
        )
        .unwrap();
    for input in [
        json!({ "verdict": { "passed": "yes" } }),
        json!({ "verdict": { "passed": false } }),
        json!({ "fail": true }),
    ] {
        let result = host
            .call("goals.verify", json!({ "id": goal["id"], "input": input }))
            .unwrap();
        assert_eq!(result["state"], "active");
        assert_eq!(result["verification"]["status"], "unachieved");
    }
    let achieved = host
        .call(
            "goals.verify",
            json!({ "id": goal["id"], "input": { "verdict": { "passed": true } } }),
        )
        .unwrap();
    assert_eq!(achieved["state"], "achieved");
    assert_eq!(achieved["verification"]["status"], "achieved");
    host.close();
}

#[test]
fn acceptance_a14_a_release_gate_blocks_on_a_missing_required_check_or_a_stale_fixture() {
    let host = verification_host(Memory::default());
    host.call(
        "gates.define",
        json!({
            "id": "release", "fixture_hash": FIXTURE_A,
            "checks": [{ "name": "unit", "required": true }, { "name": "lint", "required": false }],
        }),
    )
    .unwrap();
    let run = |variant: &str, results: Value, fixture: &str| json!({ "variant": variant, "fixture_hash": fixture, "results": results });
    let compare = |candidate: Value| {
        host.call(
            "gates.compare",
            json!({
                "gate_id": "release",
                "baseline": run("main", json!({ "unit": "passed", "lint": "passed" }), FIXTURE_A),
                "candidate": candidate,
            }),
        )
        .unwrap()
    };
    let missing = compare(run("branch", json!({ "lint": "passed" }), FIXTURE_A));
    assert_eq!(missing["decision"], "blocked");
    assert!(missing["reasons"].as_array().unwrap().contains(&json!(
        "Required check unit regressed: passed in baseline, missing in candidate"
    )));
    let stale = compare(run("branch", json!({ "unit": "passed", "lint": "passed" }), FIXTURE_B));
    assert_eq!(stale["decision"], "blocked");
    assert!(
        stale["reasons"][0]
            .as_str()
            .unwrap()
            .starts_with("Fixture mismatch: candidate branch ran against")
    );
    assert_eq!(
        compare(run("branch", json!({ "unit": "passed" }), FIXTURE_A))["decision"],
        "passed"
    );
    host.close();
}
