//! A45: saturated nested recursion makes progress or returns a bounded capacity failure, never a deadlock. Admission
//! beyond the tree limit is refused immediately (no hidden queue), a parent awaiting its children does not block
//! them, and cancelling a saturated parent cancels its waiting descendants and releases every slot.
//! Port of packages/coding-agent/test/ultron-a45-saturated-recursion.test.ts over the Rust typed-agent host and usage
//! ledger; children are spawned from the parent's lane with its task signal, as its kernel would.

mod agents_support;

use agents_support::{Fixture, Options, aborted, fixture, lock, wait_for};
use delegation::agents::Agents;
use delegation::usage::{UsageLedger, UsageLimits};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LIMIT_ERROR: &str = "Usage admitted-task limit exceeded";
/// A hang guard only (TS bounds progress at 2 s; here every wait is on a condition).
const BOUND: Duration = Duration::from_secs(120);

struct Saturated {
    fixture: Fixture,
    usage: UsageLedger,
    refusals: Arc<Mutex<Vec<String>>>,
}

fn saturated(max_admitted_tasks: u64) -> Saturated {
    let usage = UsageLedger::in_memory(UsageLimits {
        max_admitted_tasks: Some(max_admitted_tasks),
        ..UsageLimits::default()
    });
    let full = Signal::new();
    let refusals: Arc<Mutex<Vec<String>>> = Arc::default();
    let refused = refusals.clone();
    // Both children try their grandchild while the tree is full, before either ends (TS: one microtask turn).
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fixture = fixture(Options {
        usage: Some(usage.clone()),
        script: Some(Arc::new(move |lane, prompt, signal, host: &Agents| {
            let call = |kind: &str, payload: Value| host.handle(kind, payload, lane, Some(signal));
            let child = |prompt: &str| {
                call(
                    "agents.spawn",
                    json!({ "definition": "rlm-child@1", "input": { "prompt": prompt } }),
                )
            };
            if prompt == "parent" || prompt == "hanging-parent" {
                let kind = if prompt == "parent" { "child" } else { "hanging-child" };
                let children = [child(kind)?, child(kind)?];
                let refused = match call(
                    "agents.invoke",
                    json!({ "definition": "rlm-child@1", "input": { "prompt": "child" } }),
                ) {
                    Ok(_) => String::new(),
                    Err(error) => error,
                };
                let graph = call(
                    "workflows.run",
                    json!({ "nodes": [{ "id": "extra", "definition": "rlm-child@1", "input": { "prompt": "child" } }] }),
                )?;
                full.abort("saturated");
                // The parent now waits on children while the tree is full.
                let mut results = Vec::new();
                for handle in &children {
                    results.push(call("agents.result", json!({ "id": handle["id"] }))?["status"].clone());
                }
                return Ok(json!({ "refused": refused, "graph": graph["extra"], "children": results }).to_string());
            }
            if prompt == "hanging-child" {
                return aborted(signal);
            }
            while !full.wait(Duration::from_millis(5)) {}
            if let Err(error) = child("grandchild") {
                lock(&refused).push(error);
            }
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            while attempts.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                if signal.wait(Duration::from_millis(1)) {
                    return Err(signal.reason().unwrap_or_default());
                }
            }
            Ok("child done".into())
        })),
        ..Options::default()
    });
    Saturated {
        fixture,
        usage,
        refusals,
    }
}

/// Runs `f` and fails if it does not finish within the bound (a deadlock).
fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let started = Instant::now();
    let worker = std::thread::spawn(f);
    while !worker.is_finished() {
        assert!(started.elapsed() < BOUND, "no progress within {BOUND:?} (deadlock)");
        std::thread::sleep(Duration::from_millis(5));
    }
    worker.join().unwrap()
}

#[test]
fn acceptance_a45_a_parent_awaiting_children_at_saturation_gets_bounded_capacity_failures_and_still_completes() {
    let run = saturated(3);
    let agents = run.fixture.agents.clone();
    let result = within(move || {
        agents
            .handle(
                "agents.invoke",
                json!({ "definition": "rlm-child@1", "input": { "prompt": "parent" } }),
                "main",
                None,
            )
            .unwrap()
    });
    assert_eq!(result["status"], "succeeded", "{result}");
    let summary: Value = serde_json::from_str(result["value"].as_str().unwrap()).unwrap();
    // Excess admission is refused immediately with a typed capacity error, not queued.
    assert!(summary["refused"].as_str().unwrap().contains(LIMIT_ERROR));
    assert_eq!(
        (
            summary["graph"]["status"].clone(),
            summary["graph"]["verification"].clone()
        ),
        (json!("failed"), json!("unverified"))
    );
    assert!(summary["graph"]["error"].as_str().unwrap().contains(LIMIT_ERROR));
    // The children the parent was waiting for made progress and finished.
    assert_eq!(summary["children"], json!(["succeeded", "succeeded"]));
    // Grandchildren were refused while the tree was full; the children finished anyway.
    let refusals = lock(&run.refusals).clone();
    assert_eq!(refusals.len(), 2);
    for refusal in &refusals {
        assert!(refusal.contains(LIMIT_ERROR), "{refusal}");
    }
    let tasks = run.fixture.journal();
    assert_eq!(tasks.len(), 3);
    assert!(tasks.iter().all(|t| t["state"] == "completed"));
    let status = run.usage.status(None).unwrap();
    assert_eq!(
        (status["admittedTasks"].clone(), status["activeReservations"].clone()),
        (json!(0), json!(0))
    );
    run.fixture.agents.close();
}

#[test]
fn acceptance_a45_cancelling_a_saturated_parent_cancels_its_waiting_children_and_releases_every_slot() {
    let run = saturated(3);
    let parent = run
        .fixture
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "hanging-parent" } }),
        )
        .unwrap();
    wait_for("three tasks", BOUND, || run.fixture.journal().len() == 3);
    // Saturated: parent plus two hanging children hold every slot.
    assert_eq!(run.usage.status(None).unwrap()["admittedTasks"], 3);
    let refused = run
        .fixture
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "child" } }),
        )
        .unwrap_err();
    assert!(refused.contains(LIMIT_ERROR), "{refused}");

    let agents = run.fixture.agents.clone();
    let id = parent["id"].clone();
    let cancelled = within(move || {
        agents
            .handle("agents.cancel", json!({ "id": id }), "main", None)
            .unwrap()
    });
    assert_eq!(cancelled, json!({ "cancelled": true }));
    wait_for("every result", BOUND, || {
        run.fixture.journal().iter().all(|t| t.get("result").is_some())
    });
    let tasks = run.fixture.journal();
    assert_eq!(
        tasks.iter().map(|t| t["state"].clone()).collect::<Vec<_>>(),
        vec![json!("cancelled"); 3]
    );
    let parent_id = parent["id"].as_str().unwrap();
    for task in tasks.iter().filter(|t| t["id"] != parent["id"]) {
        assert_eq!(task["parentId"], parent["id"]);
        // Spawned children outlive the parent cell; the parent-task cascade stops them.
        assert_eq!(task["result"]["status"], "cancelled");
        assert!(
            task["result"]["error"]
                .as_str()
                .unwrap()
                .contains(&format!("Ancestor {parent_id} cancelled"))
        );
    }
    wait_for("three lane aborts", BOUND, || lock(&run.fixture.aborts).len() == 3);
    wait_for("slots released", BOUND, || {
        let status = run.usage.status(None).unwrap();
        status["admittedTasks"] == 0 && status["activeReservations"] == 0
    });
    // Capacity is available again.
    let next = run
        .fixture
        .call(
            "agents.spawn",
            json!({ "definition": "identity@1", "input": { "after": "cancel" } }),
        )
        .unwrap();
    assert_eq!(
        run.fixture.call("agents.result", json!({ "id": next["id"] })).unwrap()["status"],
        "succeeded"
    );
    run.fixture.agents.close();
}
