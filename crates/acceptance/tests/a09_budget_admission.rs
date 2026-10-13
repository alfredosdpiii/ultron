//! A09: concurrent calls, nested children and idempotent retries cannot bypass tree budget admission. The
//! admitted-task limit caps unfinished tasks across the whole root tree; the wall deadline caps every child deadline
//! and refuses admission once spent; missing usage and unknown pricing stay unknown (null), never zero.
//! Ports of packages/coding-agent/test/ultron-a09-budget-admission.test.ts and ultron-usage-deadline.test.ts over the
//! Rust typed-agent host and usage ledger (`delegation::usage`); a burst is real threads released together.

mod agents_support;

use agents_support::{Fixture, Options, burst, fixture, lock, wait_for};
use delegation::agents::Agents;
use delegation::usage::{Measurement, ReserveRequest, UsageLedger, UsageLimits};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIMIT_ERROR: &str = "Usage admitted-task limit exceeded";
const WAIT: Duration = Duration::from_secs(120);

struct Budget {
    fixture: Fixture,
    usage: UsageLedger,
    gate: Signal,
    nested: Arc<Mutex<Vec<Result<Value, String>>>>,
    peak: Arc<AtomicUsize>,
}

fn budget(limits: UsageLimits) -> Budget {
    let usage = UsageLedger::in_memory(limits);
    let gate = Signal::new();
    let nested: Arc<Mutex<Vec<Result<Value, String>>>> = Arc::default();
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (open, results, now, high) = (gate.clone(), nested.clone(), running.clone(), peak.clone());
    let fixture = fixture(Options {
        usage: Some(usage.clone()),
        script: Some(Arc::new(move |lane, prompt, signal, host| {
            let count = now.fetch_add(1, Ordering::SeqCst) + 1;
            high.fetch_max(count, Ordering::SeqCst);
            let outcome = (|| {
                if let Some(count) = prompt.strip_prefix("parent:") {
                    // A child lane spawns its own children through the same host, as its kernel would.
                    let count: usize = count.parse().unwrap();
                    let spawns: Vec<_> = (0..count)
                        .map(|_| {
                            let (host, lane, signal) = (host.clone(), lane.to_string(), signal.clone());
                            std::thread::spawn(move || {
                                host.handle(
                                    "agents.spawn",
                                    json!({ "definition": "rlm-child@1", "input": { "prompt": "hold" } }),
                                    &lane,
                                    Some(&signal),
                                )
                            })
                        })
                        .collect();
                    let settled: Vec<_> = spawns.into_iter().map(|t| t.join().unwrap()).collect();
                    lock(&results).extend(settled);
                }
                while !open.is_aborted() {
                    if signal.wait(Duration::from_millis(5)) {
                        return Err(signal.reason().unwrap_or_default());
                    }
                }
                Ok("done".to_string())
            })();
            now.fetch_sub(1, Ordering::SeqCst);
            outcome
        })),
        ..Options::default()
    });
    Budget {
        fixture,
        usage,
        gate,
        nested,
        peak,
    }
}

fn spawn(agents: &Agents, prompt: &str, key: Option<&str>) -> Result<Value, String> {
    let mut payload = json!({ "definition": "rlm-child@1", "input": { "prompt": prompt } });
    if let Some(key) = key {
        payload["key"] = json!(key);
    }
    agents.handle("agents.spawn", payload, "main", None)
}

fn rejected_with(results: &[Result<Value, String>], message: &str) -> usize {
    results
        .iter()
        .filter(|r| r.as_ref().err().is_some_and(|e| e.contains(message)))
        .count()
}

fn all_completed(fixture: &Fixture) -> bool {
    fixture.journal().iter().all(|t| t["state"] == "completed")
}

fn limits(max_admitted_tasks: Option<u64>, max_wall_ms: Option<u64>) -> UsageLimits {
    UsageLimits {
        max_admitted_tasks,
        max_wall_ms,
        ..UsageLimits::default()
    }
}

#[test]
fn acceptance_a09_a_burst_of_concurrent_spawns_admits_exactly_the_limit_and_rejects_the_rest() {
    let run = budget(limits(Some(3), None));
    let agents = run.fixture.agents.clone();
    let results = burst(20, move |_| spawn(&agents, "hold", None));
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
    assert_eq!(rejected_with(&results, LIMIT_ERROR), 17);
    assert_eq!(run.fixture.journal().len(), 3);
    assert_eq!(run.usage.status(None).unwrap()["admittedTasks"], 3);
    wait_for("three prompts", WAIT, || run.fixture.prompt_count() == 3);
    run.gate.abort("open");
    wait_for("all completed", WAIT, || all_completed(&run.fixture));
    assert!(run.peak.load(Ordering::SeqCst) <= 3);
    // Finished tasks release their slots; the limit is on unfinished work in the tree.
    wait_for("slots released", WAIT, || {
        run.usage.status(None).unwrap()["admittedTasks"] == 0
    });
    assert!(spawn(&run.fixture.agents, "fresh", None).unwrap()["id"].is_string());
    run.fixture.agents.close();
}

#[test]
fn acceptance_a09_nested_children_spawned_from_child_lanes_share_the_root_tree_budget() {
    let run = budget(limits(Some(4), None));
    let agents = run.fixture.agents.clone();
    let parents: Vec<Value> = burst(2, move |_| spawn(&agents, "parent:3", None).unwrap());
    wait_for("six nested spawns", WAIT, || lock(&run.nested).len() == 6);
    let nested = lock(&run.nested).clone();
    assert_eq!(nested.iter().filter(|r| r.is_ok()).count(), 2);
    assert_eq!(rejected_with(&nested, LIMIT_ERROR), 4);
    let tasks = run.fixture.journal();
    assert_eq!(tasks.len(), 4);
    let children: Vec<&Value> = tasks.iter().filter(|t| t.get("parentId").is_some()).collect();
    assert_eq!(children.len(), 2);
    let parent_ids: Vec<&Value> = parents.iter().map(|p| &p["id"]).collect();
    for child in children {
        assert!(parent_ids.contains(&&child["parentId"]));
    }
    assert_eq!(run.usage.status(None).unwrap()["admittedTasks"], 4);
    // The root is refused too while its descendants hold the tree's slots.
    assert!(
        spawn(&run.fixture.agents, "hold", None)
            .unwrap_err()
            .contains(LIMIT_ERROR)
    );
    run.gate.abort("open");
    wait_for("all completed", WAIT, || all_completed(&run.fixture));
    wait_for("slots released", WAIT, || {
        run.usage.status(None).unwrap()["admittedTasks"] == 0
    });
    run.fixture.agents.close();
}

#[test]
fn acceptance_a09_idempotent_retries_neither_consume_a_slot_nor_release_the_live_originals_slot() {
    let run = budget(limits(Some(2), None));
    let original = spawn(&run.fixture.agents, "hold", Some("retry-key")).unwrap();
    spawn(&run.fixture.agents, "hold", None).unwrap();
    let agents = run.fixture.agents.clone();
    let retries = burst(10, move |_| spawn(&agents, "hold", Some("retry-key")).unwrap());
    for retry in &retries {
        assert_eq!(retry["id"], original["id"]);
    }
    // Retrying must not have settled the original's reservation: the tree is still full.
    assert_eq!(run.usage.status(None).unwrap()["admittedTasks"], 2);
    assert!(
        spawn(&run.fixture.agents, "hold", None)
            .unwrap_err()
            .contains(LIMIT_ERROR)
    );
    assert!(
        spawn(&run.fixture.agents, "different input", Some("retry-key"))
            .unwrap_err()
            .contains("Idempotency key reused")
    );
    assert_eq!(run.fixture.journal().len(), 2);
    run.gate.abort("open");
    wait_for("all completed", WAIT, || all_completed(&run.fixture));
    wait_for("slots released", WAIT, || {
        run.usage.status(None).unwrap()["admittedTasks"] == 0
    });
    let before = run.usage.status(None).unwrap()["usage"].clone();
    // A retry after completion returns the durable result and records no new task call.
    assert_eq!(
        run.fixture
            .call(
                "agents.invoke",
                json!({ "definition": "rlm-child@1", "input": { "prompt": "hold" }, "key": "retry-key" })
            )
            .unwrap(),
        json!({ "status": "succeeded", "value": "done", "verification": "unverified", "verdict": null, "unverified": true })
    );
    assert_eq!(run.usage.status(None).unwrap()["usage"], before);
    assert_eq!(
        lock(&run.fixture.prompts).iter().filter(|(_, p)| p == "hold").count(),
        2
    );
    run.fixture.agents.close();
}

#[test]
fn acceptance_a09_the_root_wall_deadline_caps_child_deadlines_and_refuses_admission_once_spent() {
    let run = budget(limits(None, Some(150)));
    // The default 30-minute task timeout is capped by the 150ms root deadline, not refused.
    let first = spawn(&run.fixture.agents, "hold", None).unwrap();
    let second = spawn(&run.fixture.agents, "hold", None).unwrap();
    for task in [first, second] {
        assert_eq!(
            run.fixture.call("agents.result", json!({ "id": task["id"] })).unwrap(),
            json!({ "status": "cancelled", "error": "Ultron root wall deadline exceeded", "verification": "unverified" })
        );
    }
    assert!(
        spawn(&run.fixture.agents, "hold", None)
            .unwrap_err()
            .contains("Usage wall deadline exceeded")
    );
    let status = run.usage.status(None).unwrap();
    assert_eq!(status["remainingWallMs"], 0);
    assert_eq!(status["activeReservations"], 0);
    assert_eq!(status["usage"]["taskCalls"], 2);
    run.fixture.agents.close();
}

#[test]
fn acceptance_a09_missing_usage_and_unknown_pricing_stay_unknown_never_zero() {
    let run = budget(UsageLimits::default());
    run.gate.abort("open");
    let task = spawn(&run.fixture.agents, "hold", None).unwrap();
    run.fixture.call("agents.result", json!({ "id": task["id"] })).unwrap();
    // The fake lane reports no usage: task and model calls are recorded as unknown.
    let recorded = run.usage.status(None).unwrap()["usage"].clone();
    for (key, want) in [
        ("calls", json!(2)),
        ("taskCalls", json!(1)),
        ("modelCalls", json!(1)),
        ("unknownCalls", json!(2)),
    ] {
        assert_eq!(recorded[key], want, "{key}");
    }
    for key in ["inputTokens", "outputTokens", "totalTokens", "cost"] {
        assert_eq!(recorded[key], Value::Null, "{key}");
    }
    run.fixture.agents.close();

    let ledger = UsageLedger::in_memory(UsageLimits::default());
    let priced = ledger.reserve(&ReserveRequest::model("priced")).unwrap();
    let unpriced = ledger.reserve(&ReserveRequest::model("unpriced")).unwrap();
    let measured = |input: f64, output: f64, total: f64, cost: Option<f64>| Measurement {
        input_tokens: Some(input),
        output_tokens: Some(output),
        total_tokens: Some(total),
        cost,
        wall_ms: None,
    };
    ledger
        .settle(&priced, Some("succeeded"), Some(&measured(10.0, 5.0, 15.0, Some(0.01))))
        .unwrap();
    // Known tokens, unknown price: tokens total, cost stays unresolved for the whole root.
    ledger
        .settle(&unpriced, Some("succeeded"), Some(&measured(20.0, 5.0, 25.0, None)))
        .unwrap();
    let usage = ledger.status(None).unwrap()["usage"].clone();
    for (key, want) in [
        ("inputTokens", json!(30)),
        ("outputTokens", json!(10)),
        ("totalTokens", json!(40)),
        ("cost", Value::Null),
        ("unknownCalls", json!(1)),
    ] {
        assert_eq!(usage[key], want, "{key}");
    }
    let invalid = ledger.reserve(&ReserveRequest::model("invalid")).unwrap();
    let nan = Measurement {
        cost: Some(f64::NAN),
        ..Measurement::default()
    };
    assert_eq!(
        ledger.settle(&invalid, None, Some(&nan)).unwrap_err(),
        "Invalid usage ledger cost"
    );
}

#[test]
fn acceptance_a09_direct_ledger_reservations_race_to_exactly_the_limit() {
    let ledger = UsageLedger::in_memory(limits(Some(5), None));
    let racing = ledger.clone();
    let results = burst(50, move |i| racing.reserve(&ReserveRequest::task(format!("task-{i}"))));
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 5);
    assert_eq!(
        results
            .iter()
            .filter(|r| r.as_ref().err().is_some_and(|e| e.contains(LIMIT_ERROR)))
            .count(),
        45
    );
    // A retried request key reuses its reservation instead of taking another slot; task-0 may have lost the race,
    // so retry one that won.
    let won = results.iter().flatten().next().unwrap().0["requestKey"].clone();
    let retried = ledger.reserve(&ReserveRequest::task(won.as_str().unwrap())).unwrap();
    assert_eq!(retried.0["requestKey"], won);
    assert_eq!(ledger.status(None).unwrap()["admittedTasks"], 5);
}

#[test]
fn acceptance_a09_later_tasks_with_the_default_timeout_are_capped_to_the_remaining_root_budget_not_refused() {
    let ledger = UsageLedger::in_memory(limits(None, Some(30 * 60 * 1000)));
    let timed = |key: &str| ReserveRequest {
        timeout_ms: Some(30.0 * 60.0 * 1000.0),
        ..ReserveRequest::task(key)
    };
    let first = ledger.reserve(&timed("a")).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let second = ledger.reserve(&timed("b")).unwrap();
    assert_eq!(second.deadline_at(), first.deadline_at());
    let untimed = ledger.reserve(&ReserveRequest::task("c")).unwrap();
    assert_eq!(untimed.deadline_at(), first.deadline_at());
}
