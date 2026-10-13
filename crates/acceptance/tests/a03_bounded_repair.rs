//! A03: an invalid result triggers bounded repair (predict) or a typed failure (every strategy). Repairs are bounded
//! by maxRepairs (itself capped at 2), visible to the adapter and in the usage ledger, and exhaustion returns a typed
//! failure instead of the last malformed answer.
//! Ports of packages/coding-agent/test/ultron-a03-bounded-repair.test.ts and ultron-predict-adapter.test.ts over the
//! Rust typed-agent host and its production predict adapter (`delegation::agents::predict_adapter`).
//! Divergence: a Rust adapter cannot return a non-JSON value (TS NaN); the non-JSON case goes through the
//! production predict adapter, whose unparsable reply is returned as text and fails validation the same way.

mod agents_support;

use agents_support::{Options, adapter, definition, fixture, input_of, lock};
use delegation::agents::{Complete, Completion, PredictCall, predict_adapter};
use delegation::usage::{UsageLedger, UsageLimits};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// `doubled` must be a nonnegative integer: -4 is well-typed but violates a declared constraint.
fn constrained(max_repairs: u64) -> Value {
    json!({
        "maxRepairs": max_repairs,
        "outputSchema": { "type": "object", "properties": { "doubled": { "type": "integer", "minimum": 0 } },
                          "required": ["doubled"], "additionalProperties": false },
    })
}

struct Predict {
    fixture: agents_support::Fixture,
    calls: Arc<Mutex<Vec<Value>>>,
    usage: UsageLedger,
}

/// A predict adapter answering `answers[attempt]` (the last one repeats); `Err` answers are thrown.
fn predict(answers: Vec<Result<Value, String>>, max_repairs: u64) -> Predict {
    let calls: Arc<Mutex<Vec<Value>>> = Arc::default();
    let usage = UsageLedger::in_memory(UsageLimits::default());
    let seen = calls.clone();
    let fixture = fixture(Options {
        usage: Some(usage.clone()),
        predict: Some(adapter(move |_, _, repair, _| {
            let mut calls = lock(&seen);
            calls.push(repair.cloned().unwrap_or(Value::Null));
            answers[(calls.len() - 1).min(answers.len() - 1)].clone()
        })),
        ..Options::default()
    });
    fixture
        .call(
            "agents.register",
            json!({ "definition": definition("judge", "predict", constrained(max_repairs)) }),
        )
        .unwrap();
    Predict { fixture, calls, usage }
}

#[test]
fn acceptance_a03_predict_repairs_a_semantically_invalid_result_within_max_repairs_and_reports_the_repair() {
    let run = predict(vec![Ok(json!({ "doubled": -4 })), Ok(json!({ "doubled": 4 }))], 2);
    let result = run
        .fixture
        .call("agents.invoke", json!({ "definition": "judge@1", "input": { "n": 2 } }))
        .unwrap();
    assert_eq!(
        result,
        json!({ "status": "succeeded", "value": { "doubled": 4 }, "verification": "unverified" })
    );
    assert_eq!(
        *lock(&run.calls),
        vec![
            Value::Null,
            json!({ "attempt": 1, "previous": { "doubled": -4 }, "error": "judge@1 output does not match its schema" })
        ]
    );
    // Observable: one model call per attempt, all charged to the same root and settled.
    let status = run.usage.status(None).unwrap();
    assert_eq!(status["usage"]["taskCalls"], 1);
    assert_eq!(status["usage"]["modelCalls"], 2);
    assert_eq!(status["activeReservations"], 0);
    run.fixture.agents.close();
}

#[test]
fn acceptance_a03_predict_stops_after_max_repairs_and_fails_typed_without_returning_the_malformed_answer() {
    let run = predict(vec![Ok(json!({ "doubled": -4 }))], 2);
    let result = run
        .fixture
        .call("agents.invoke", json!({ "definition": "judge@1", "input": { "n": 2 } }))
        .unwrap();
    assert_eq!(
        result,
        json!({ "status": "failed", "error": "judge@1 output does not match its schema after 2 repair attempts",
                "verification": "unverified" })
    );
    assert!(result.get("value").is_none());
    let attempts: Vec<Value> = lock(&run.calls)
        .iter()
        .map(|r| r.get("attempt").cloned().unwrap_or(json!(0)))
        .collect();
    assert_eq!(attempts, vec![json!(0), json!(1), json!(2)]);
    let journal = run.fixture.journal();
    assert_eq!(
        (journal[0]["state"].clone(), journal[0]["result"].clone()),
        (json!("failed"), result)
    );
    let status = run.usage.status(None).unwrap();
    assert_eq!(
        (
            status["usage"]["taskCalls"].clone(),
            status["usage"]["modelCalls"].clone()
        ),
        (json!(1), json!(3))
    );
    run.fixture.agents.close();
}

#[test]
fn acceptance_a03_max_repairs_0_makes_exactly_one_attempt_and_more_than_2_repairs_cannot_be_registered() {
    let run = predict(vec![Ok(json!({ "doubled": "four" }))], 0);
    let result = run
        .fixture
        .call("agents.invoke", json!({ "definition": "judge@1", "input": { "n": 2 } }))
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"], "judge@1 output does not match its schema");
    assert_eq!(lock(&run.calls).len(), 1);
    let greedy = definition("greedy", "predict", json!({ "maxRepairs": 3 }));
    assert_eq!(
        run.fixture
            .call("agents.register", json!({ "definition": greedy }))
            .unwrap_err(),
        "Invalid native agent definition"
    );
    run.fixture.agents.close();
}

/// The production predict adapter over scripted model replies; records every request's user prompt.
fn scripted(replies: Vec<&'static str>) -> (Complete, Arc<Mutex<Vec<String>>>) {
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = requests.clone();
    let replies = Arc::new(Mutex::new(replies.into_iter()));
    let complete: Complete = Arc::new(move |call: PredictCall<'_>| {
        lock(&seen).push(call.user);
        Ok(Completion {
            text: lock(&replies).next().unwrap_or("{}").to_string(),
            stop_reason: "stop".into(),
            error_message: None,
        })
    });
    (complete, requests)
}

#[test]
fn acceptance_a03_non_json_and_throwing_predict_adapters_fail_typed() {
    // Non-JSON: the model's unparsable reply is not a valid output, so it is repaired, then fails typed.
    let (complete, requests) = scripted(vec!["NaN", "NaN"]);
    let fixture = fixture(Options {
        predict: Some(predict_adapter(complete)),
        ..Options::default()
    });
    fixture
        .call(
            "agents.register",
            json!({ "definition": definition("judge", "predict", constrained(1)) }),
        )
        .unwrap();
    let result = fixture
        .call("agents.invoke", json!({ "definition": "judge@1", "input": { "n": 1 } }))
        .unwrap();
    assert_eq!(
        (result["status"].clone(), result["verification"].clone()),
        (json!("failed"), json!("unverified"))
    );
    assert_eq!(lock(&requests).len(), 2);
    fixture.agents.close();
    // Throwing: a provider error is not a validation failure and is not silently retried.
    let throwing = predict(vec![Err("provider refused".into())], 2);
    assert_eq!(
        throwing
            .fixture
            .call("agents.invoke", json!({ "definition": "judge@1", "input": { "n": 1 } }))
            .unwrap(),
        json!({ "status": "failed", "error": "provider refused", "verification": "unverified" })
    );
    assert_eq!(lock(&throwing.calls).len(), 1);
    throwing.fixture.agents.close();
}

#[test]
fn acceptance_a03_rlm_malformed_json_and_schema_invalid_output_fail_typed_after_one_bounded_attempt() {
    let fixture = fixture(Options {
        script: Some(Arc::new(|_, prompt, _, _| {
            Ok(match input_of(prompt)["n"].as_i64().unwrap() {
                1 => "this is not json {".into(),
                2 => r#"{"doubled":-4}"#.into(),
                _ => "```json\n{\"doubled\":\"six\"}\n```".into(),
            })
        })),
        ..Options::default()
    });
    // maxRepairs is declared but the rlm strategy never loops on a bad answer: one prompt per task.
    fixture
        .call(
            "agents.register",
            json!({ "definition": definition("investigator", "rlm", constrained(2)) }),
        )
        .unwrap();
    let malformed = fixture
        .call(
            "agents.invoke",
            json!({ "definition": "investigator@1", "input": { "n": 1 } }),
        )
        .unwrap();
    assert_eq!(
        (malformed["status"].clone(), malformed["verification"].clone()),
        (json!("failed"), json!("unverified"))
    );
    assert!(malformed["error"].as_str().unwrap().contains("JSON"), "{malformed}");
    assert!(malformed.get("value").is_none());
    for n in [2, 3] {
        assert_eq!(
            fixture
                .call(
                    "agents.invoke",
                    json!({ "definition": "investigator@1", "input": { "n": n } })
                )
                .unwrap(),
            json!({ "status": "failed", "error": "investigator@1 output does not match its schema",
                    "verification": "unverified" })
        );
    }
    assert_eq!(fixture.prompt_count(), 3);
    let states: Vec<Value> = fixture.journal().iter().map(|t| t["state"].clone()).collect();
    assert_eq!(states, vec![json!("failed"); 3]);
    fixture.agents.close();
}

fn classify() -> Value {
    json!({
        "id": "classify-severity", "version": "1", "strategy": "predict",
        "instructions": "Classify the severity of the reported defect.",
        "inputSchema": { "type": "object", "required": ["report"], "properties": { "report": { "type": "string" } } },
        "outputSchema": { "type": "object", "required": ["severity"], "additionalProperties": false,
                          "properties": { "severity": { "enum": ["low", "high"] } } },
        "maxRepairs": 1, "inputDescription": "A defect report", "outputDescription": "{severity: low|high}",
    })
}

#[test]
fn acceptance_a03_predict_adapter_one_call_per_attempt_repair_with_the_validation_error_no_lane_or_kernel() {
    let (complete, requests) = scripted(vec![
        r#"{"severity": "urgent"}"#,
        "```json\n{\"severity\": \"high\"}\n```",
    ]);
    let fixture = fixture(Options {
        predict: Some(predict_adapter(complete)),
        ..Options::default()
    });
    fixture
        .call("agents.register", json!({ "definition": classify() }))
        .unwrap();
    let result = fixture
        .call(
            "agents.invoke",
            json!({ "definition": "classify-severity@1", "input": { "report": "data loss on save" } }),
        )
        .unwrap();
    assert_eq!(
        result,
        json!({ "status": "succeeded", "value": { "severity": "high" }, "verification": "unverified" })
    );
    let requests = lock(&requests).clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("data loss on save"));
    assert!(requests[1].contains("rejected (attempt 1)"));
    assert!(lock(&fixture.lane_calls).is_empty());
    fixture.agents.close();
}

#[test]
fn acceptance_a03_predict_adapter_exhausted_repairs_fail_typed_instead_of_claiming_success() {
    let (complete, requests) = scripted(vec!["not json", r#"{"severity": "medium"}"#, r#"{"severity": "high"}"#]);
    let fixture = fixture(Options {
        predict: Some(predict_adapter(complete)),
        ..Options::default()
    });
    fixture
        .call("agents.register", json!({ "definition": classify() }))
        .unwrap();
    let result = fixture
        .call(
            "agents.invoke",
            json!({ "definition": "classify-severity@1", "input": { "report": "typo" } }),
        )
        .unwrap();
    assert_eq!(
        (result["status"].clone(), result["verification"].clone()),
        (json!("failed"), json!("unverified"))
    );
    // maxRepairs 1 bounds the attempts to two.
    assert_eq!(lock(&requests).len(), 2);
    fixture.agents.close();
}
