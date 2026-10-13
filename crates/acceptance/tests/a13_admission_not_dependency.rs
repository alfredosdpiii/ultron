//! A13: admitting (or starting) a task does not satisfy a graph dependency. A dependent is neither admitted nor
//! started until its dependency's terminal result is durably committed, and a dependency that ends without success
//! skips its dependent even if a valid result arrives late.
//! Port of packages/coding-agent/test/ultron-a13-admission-not-dependency.test.ts over the Rust typed-agent host.

mod agents_support;

use agents_support::{Fixture, Memory, Options, adapter, fixture, lock, wait_for};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Event {
    event: String,
    at: Instant,
    slow_stored: Option<String>,
}

struct Slow {
    fixture: Fixture,
    release: Arc<Mutex<Option<String>>>,
    events: Arc<Mutex<Vec<Event>>>,
}

fn slow() -> Slow {
    let release: Arc<Mutex<Option<String>>> = Arc::default();
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let store = Memory::default();
    let (reply, log) = (release.clone(), events.clone());
    let (journal, seen) = (store.clone(), events.clone());
    let fixture = fixture(Options {
        store: Some(store),
        script: Some(Arc::new(move |_, prompt, signal, _| {
            let event = |name: &str| Event {
                event: format!("{prompt}:{name}"),
                at: Instant::now(),
                slow_stored: None,
            };
            lock(&log).push(event("start"));
            loop {
                if let Some(text) = lock(&reply).clone() {
                    lock(&log).push(event("reply"));
                    return Ok(text);
                }
                if signal.wait(Duration::from_millis(2)) {
                    return Err(signal.reason().unwrap_or_default());
                }
            }
        })),
        deterministic: Some(adapter(move |_, input, _, _| {
            // What the durable journal says about the dependency at the moment the dependent starts.
            let stored = journal.document().and_then(|d| {
                d["tasks"]
                    .as_array()
                    .and_then(|tasks| tasks.iter().find(|t| t["definition"] == "rlm-child@1").cloned())
            });
            lock(&seen).push(Event {
                event: "dependent:start".into(),
                at: Instant::now(),
                slow_stored: stored.and_then(|t| t["state"].as_str().map(String::from)),
            });
            Ok(input.clone())
        })),
        ..Options::default()
    });
    Slow {
        fixture,
        release,
        events,
    }
}

/// `settle()`: a short pause for anything that would wrongly start.
fn settle() {
    std::thread::sleep(Duration::from_millis(40));
}

fn names(events: &Arc<Mutex<Vec<Event>>>) -> Vec<String> {
    lock(events).iter().map(|e| e.event.clone()).collect()
}

#[test]
fn acceptance_a13_a_dependent_is_not_admitted_or_started_until_the_slow_dependencys_result_is_durable() {
    let run = slow();
    let agents = run.fixture.agents.clone();
    let workflow = std::thread::spawn(move || {
        agents.handle(
            "workflows.run",
            json!({ "nodes": [
                { "id": "slow", "definition": "rlm-child@1", "input": { "prompt": "slow" } },
                { "id": "dependent", "definition": "identity@1", "dependsOn": ["slow"], "inputFrom": "slow" },
            ] }),
            "main",
            None,
        )
    });
    wait_for("slow started", Duration::from_secs(120), || {
        names(&run.events).contains(&"slow:start".to_string())
    });
    settle();
    // The dependency is admitted and running, which is not enough to start the dependent.
    let during = run.fixture.journal();
    assert_eq!(during.len(), 1);
    assert_eq!(
        (during[0]["definition"].clone(), during[0]["state"].clone()),
        (json!("rlm-child@1"), json!("running"))
    );
    assert!(during[0].get("result").is_none());
    assert_eq!(names(&run.events), ["slow:start"]);

    *lock(&run.release) = Some("delayed valid result".into());
    let output = workflow.join().unwrap().unwrap();
    // A subagent that ended without rlm.finish returns its reply unverified.
    assert_eq!(
        output["slow"],
        json!({ "status": "succeeded", "value": "delayed valid result", "verification": "unverified", "verdict": null,
                "unverified": true })
    );
    assert_eq!(
        output["dependent"],
        json!({ "status": "succeeded", "value": "delayed valid result", "verification": "unverified" })
    );
    assert_eq!(names(&run.events), ["slow:start", "slow:reply", "dependent:start"]);
    let events = lock(&run.events).clone();
    assert!(events[2].at >= events[1].at);
    // When the dependent started, the dependency's terminal result was already committed.
    assert_eq!(events[2].slow_stored.as_deref(), Some("completed"));
    run.fixture.agents.close();
}

#[test]
fn acceptance_a13_agents_spawn_confirms_admission_only_the_result_is_not_available_until_the_task_ends() {
    let run = slow();
    let handle = run
        .fixture
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "slow" } }),
        )
        .unwrap();
    assert_eq!(handle["state"], "admitted");
    let agents = run.fixture.agents.clone();
    let id = handle["id"].clone();
    let result = std::thread::spawn(move || agents.handle("agents.result", json!({ "id": id }), "main", None));
    settle();
    assert!(!result.is_finished());
    let inspected = run
        .fixture
        .call("agents.inspect", json!({ "id": handle["id"] }))
        .unwrap();
    assert!(inspected.get("result").is_none(), "{inspected}");
    *lock(&run.release) = Some("late but valid".into());
    assert_eq!(
        result.join().unwrap().unwrap(),
        json!({ "status": "succeeded", "value": "late but valid", "verification": "unverified", "verdict": null,
                "unverified": true })
    );
    run.fixture.agents.close();
}

#[test]
fn acceptance_a13_a_dependency_that_ends_unsuccessfully_skips_its_dependent_and_a_late_valid_result_cannot_revive_it() {
    let run = slow();
    let output = run
        .fixture
        .call(
            "workflows.run",
            json!({ "nodes": [
                { "id": "slow", "definition": "rlm-child@1", "input": { "prompt": "slow" }, "timeout_ms": 30 },
                { "id": "dependent", "definition": "identity@1", "dependsOn": ["slow"], "inputFrom": "slow" },
            ] }),
        )
        .unwrap();
    assert_eq!(
        (output["slow"]["status"].clone(), output["slow"]["verification"].clone()),
        (json!("cancelled"), json!("unverified"))
    );
    assert_eq!(
        output["dependent"],
        json!({ "status": "skipped", "reason": "Dependency slow did not succeed (cancelled)" })
    );
    *lock(&run.release) = Some("too late".into());
    settle();
    assert!(!names(&run.events).contains(&"dependent:start".to_string()));
    let tasks: Vec<Value> = run.fixture.journal();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        (tasks[0]["state"].clone(), tasks[0]["result"]["status"].clone()),
        (json!("cancelled"), json!("cancelled"))
    );
    run.fixture.agents.close();
}
