//! A35: deterministic, predict and rlm methods share policy, validation, accounting and task semantics. The same typed
//! definition implemented by each strategy gets the same idempotency, timeout/cancel, usage reservation and journal
//! behavior; only rlm launches an iterative lane.
//! Port of packages/coding-agent/test/ultron-a35-strategy-parity.test.ts over the Rust typed-agent host, its usage
//! ledger and the TS fixture's scripted adapters and lane.

mod agents_support;

use agents_support::{Fixture, Memory, Options, aborted, adapter, definition, fixture, input_of, lock};
use delegation::usage::{UsageLedger, UsageOptions};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const STRATEGIES: [(&str, &str); 3] = [("deterministic", "det@1"), ("predict", "pred@1"), ("rlm", "loop@1")];

/// n doubles; negative n answers schema-invalid; n = 999 hangs until the task is aborted.
fn answer(n: i64, signal: &Signal) -> Result<Value, String> {
    if n == 999 {
        return aborted(signal).map(Value::from);
    }
    Ok(if n < 0 {
        json!({ "doubled": "invalid" })
    } else {
        json!({ "doubled": n * 2 })
    })
}

struct Parity {
    fixture: Fixture,
    /// (strategy, the task's signal) of every adapter call.
    adapter_calls: Arc<Mutex<Vec<(String, Signal)>>>,
    usage: UsageLedger,
    usage_document: Memory,
    store: Memory,
}

fn parity() -> Parity {
    let adapter_calls: Arc<Mutex<Vec<(String, Signal)>>> = Arc::default();
    let usage_document = Memory::default();
    let usage = UsageLedger::new(Box::new(usage_document.clone()), UsageOptions::default()).unwrap();
    let store = Memory::default();
    let strategy = |name: &'static str| {
        let calls = adapter_calls.clone();
        adapter(move |_, input, _, signal| {
            lock(&calls).push((name.to_string(), signal.clone()));
            answer(input["n"].as_i64().unwrap(), signal)
        })
    };
    let fixture = fixture(Options {
        store: Some(store.clone()),
        usage: Some(usage.clone()),
        deterministic: Some(strategy("deterministic")),
        predict: Some(strategy("predict")),
        script: Some(Arc::new(|_, prompt, signal, _| {
            answer(input_of(prompt)["n"].as_i64().unwrap(), signal).map(|v| v.to_string())
        })),
        ..Options::default()
    });
    for (strategy, key) in STRATEGIES {
        let id = key.split('@').next().unwrap();
        fixture
            .call(
                "agents.register",
                json!({ "definition": definition(id, strategy, json!({})) }),
            )
            .unwrap();
    }
    Parity {
        fixture,
        adapter_calls,
        usage,
        usage_document,
        store,
    }
}

fn invoke(fixture: &Fixture, payload: Value) -> Result<Value, String> {
    fixture.call("agents.invoke", payload)
}

#[test]
fn acceptance_a35_only_rlm_launches_a_lane_predict_and_deterministic_never_start_an_iterative_kernel() {
    let run = parity();
    for (_, key) in STRATEGIES {
        for n in [2, -1] {
            invoke(&run.fixture, json!({ "definition": key, "input": { "n": n } })).unwrap();
        }
        invoke(
            &run.fixture,
            json!({ "definition": key, "input": { "n": 999 }, "timeout_ms": 10 }),
        )
        .unwrap();
    }
    let lanes = lock(&run.fixture.lane_calls).clone();
    assert_eq!(lanes.len(), 3);
    for lane in &lanes {
        assert!(lane.starts_with("ultron.loop.ultron-task-"), "{lane}");
    }
    let strategies: Vec<String> = lock(&run.adapter_calls).iter().map(|(s, _)| s.clone()).collect();
    assert_eq!(
        strategies,
        [
            "deterministic",
            "deterministic",
            "deterministic",
            "predict",
            "predict",
            "predict"
        ]
    );
    run.fixture.agents.close();
}

#[test]
fn acceptance_a35_the_same_validation_and_result_contract_applies_to_every_strategy() {
    let run = parity();
    for (_, key) in STRATEGIES {
        let id = key.split('@').next().unwrap();
        assert_eq!(
            invoke(&run.fixture, json!({ "definition": key, "input": { "n": 4 } })).unwrap(),
            json!({ "status": "succeeded", "value": { "doubled": 8 }, "verification": "unverified" })
        );
        assert_eq!(
            invoke(&run.fixture, json!({ "definition": key, "input": { "n": -1 } })).unwrap(),
            json!({ "status": "failed", "error": format!("{id}@1 output does not match its schema"),
                    "verification": "unverified" })
        );
        assert_eq!(
            invoke(&run.fixture, json!({ "definition": key, "input": { "n": "4" } })).unwrap_err(),
            format!("{id}@1 input does not match its schema")
        );
    }
    assert_eq!(run.fixture.journal().len(), 6);
    run.fixture.agents.close();
}

#[test]
fn acceptance_a35_idempotency_keys_behave_identically_across_strategies() {
    let run = parity();
    let fixture = &run.fixture;
    for (strategy, key) in STRATEGIES {
        let request = json!({ "definition": key, "input": { "n": 3 }, "key": format!("same-{strategy}") });
        let both: Vec<Value> = [0, 1]
            .map(|_| {
                let (agents, request) = (fixture.agents.clone(), request.clone());
                std::thread::spawn(move || agents.handle("agents.invoke", request, "main", None).unwrap())
            })
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert_eq!(both[0], both[1]);
        assert_eq!(invoke(fixture, request.clone()).unwrap(), both[0]);
        let mut other = request.clone();
        other["input"] = json!({ "n": 4 });
        assert_eq!(
            invoke(fixture, other).unwrap_err(),
            "Idempotency key reused for a different task"
        );
    }
    assert_eq!(fixture.journal().len(), 3);
    assert_eq!(lock(&run.adapter_calls).len(), 2);
    assert_eq!(fixture.prompt_count(), 1);
    fixture.agents.close();
}

#[test]
fn acceptance_a35_timeout_and_cancellation_commit_the_same_cancelled_result_and_abort_the_work_for_every_strategy() {
    let run = parity();
    for (_, key) in STRATEGIES {
        assert_eq!(
            invoke(
                &run.fixture,
                json!({ "definition": key, "input": { "n": 999 }, "timeout_ms": 10 })
            )
            .unwrap(),
            json!({ "status": "cancelled", "error": "Ultron task exceeded 10ms timeout", "verification": "unverified" })
        );
        let spawned = run
            .fixture
            .call("agents.spawn", json!({ "definition": key, "input": { "n": 999 } }))
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            run.fixture
                .call("agents.cancel", json!({ "id": spawned["id"] }))
                .unwrap(),
            json!({ "cancelled": true })
        );
        assert_eq!(
            run.fixture
                .call("agents.result", json!({ "id": spawned["id"] }))
                .unwrap(),
            json!({ "status": "cancelled", "error": "Ultron task cancelled", "verification": "unverified" })
        );
    }
    for (strategy, signal) in lock(&run.adapter_calls).iter() {
        assert!(signal.is_aborted(), "{strategy}");
    }
    assert_eq!(lock(&run.fixture.aborts).len(), 2);
    run.fixture.agents.close();
}

#[test]
fn acceptance_a35_usage_reservations_and_durable_journal_transitions_match_across_strategies() {
    let run = parity();
    let mut ids: Vec<(String, Vec<String>)> = Vec::new();
    for (strategy, key) in STRATEGIES {
        let mut own = Vec::new();
        for n in [5, -5] {
            let spawned = run
                .fixture
                .call("agents.spawn", json!({ "definition": key, "input": { "n": n } }))
                .unwrap();
            run.fixture
                .call("agents.result", json!({ "id": spawned["id"] }))
                .unwrap();
            own.push(spawned["id"].as_str().unwrap().to_string());
        }
        ids.push((strategy.to_string(), own));
    }
    // Every task, whatever its strategy, is admitted, runs, and ends durably.
    for (_, own) in &ids {
        assert_eq!(run.store.states_of(&own[0]), ["admitted", "running", "completed"]);
        assert_eq!(run.store.states_of(&own[1]), ["admitted", "running", "failed"]);
    }
    // One task reservation per task, settled with the task's outcome; model calls are charged for the strategies
    // that infer (predict, rlm) and never for deterministic code.
    let document = run.usage_document.document().unwrap();
    let calls = document["roots"]["ultron-root"]["calls"].as_array().unwrap().clone();
    let task_statuses: Vec<&str> = calls
        .iter()
        .filter(|c| c["kind"] == "task")
        .map(|c| c["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        task_statuses,
        ["succeeded", "failed", "succeeded", "failed", "succeeded", "failed"]
    );
    let model_calls_for = |tasks: &[String]| {
        calls
            .iter()
            .filter(|c| c["kind"] == "model")
            .filter(|c| {
                let key = c["requestKey"].as_str().unwrap_or_default();
                tasks.iter().any(|id| key.starts_with(&format!("{id}:model")))
            })
            .count()
    };
    assert_eq!(model_calls_for(&ids[0].1), 0);
    assert_eq!(model_calls_for(&ids[1].1), 2);
    assert_eq!(model_calls_for(&ids[2].1), 2);
    let status = run.usage.status(None).unwrap();
    assert_eq!(status["activeReservations"], 0);
    assert_eq!(
        (
            status["usage"]["taskCalls"].clone(),
            status["usage"]["modelCalls"].clone()
        ),
        (json!(6), json!(4))
    );
    run.fixture.agents.close();
}
