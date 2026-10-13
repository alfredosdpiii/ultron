//! A06 (partial, one more case): "a task past its timeout is cancelled with a durable cancelled result" of
//! packages/coding-agent/test/ultron-a06-limits.test.ts. The TS case spawns a definition through `agents.spawn`; the
//! Rust task host for spawned work is the subagent host (`crates/delegation`, `rlm.spawn` with `timeout_ms=`), whose
//! timeout commits the same cancelled result, and a host reopened over the same journal reports it.

use delegation::host::{ChildRunner, CollectMode, Delegation, DelegationOptions};
use delegation::task_store::TaskStore;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Store(Arc<Mutex<Option<Value>>>);

impl TaskStore for Store {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

fn host(store: &Store, runner: ChildRunner) -> Delegation {
    let mut options = DelegationOptions::new(runner);
    options.store = Box::new(store.clone());
    options.collect = CollectMode::Wait;
    Delegation::new(options).0
}

#[test]
fn acceptance_a06_task_past_its_timeout_is_cancelled_with_a_durable_cancelled_result() {
    let store = Store::default();
    // A child that never answers on its own: it ends only when its signal is aborted.
    let runner: ChildRunner = Arc::new(|run| {
        while !run.signal.wait(Duration::from_millis(20)) {}
        Err("aborted".into())
    });
    let first = host(&store, runner.clone());
    let spawned = first
        .handle(
            "rlm.spawn",
            json!({"prompt": "review", "kwargs": {"name": "reviewer", "timeout_ms": 50}}),
            "main",
        )
        .unwrap();
    let id = spawned["rlm_child_id"].as_str().unwrap().to_string();
    assert_eq!(spawned["timeout_ms"], 50);
    let waited = first.wait(std::slice::from_ref(&id), Some(Duration::from_secs(120)));
    let result = waited[0].1.as_ref().unwrap();
    assert_eq!(result["status"], "cancelled", "{result}");
    assert_eq!(result["error"], "Ultron task exceeded 50ms timeout");
    first.close();

    // A new host over the same journal sees the committed terminal result.
    let reopened = host(&store, runner);
    let task = reopened
        .tasks()
        .unwrap()
        .into_iter()
        .find(|t| t["id"] == id.as_str())
        .unwrap();
    assert_eq!(task["state"], "cancelled", "{task}");
    assert_eq!(task["result"]["status"], "cancelled");
    assert_eq!(task["result"]["error"], "Ultron task exceeded 50ms timeout");
    reopened.close();

    // The range is bounded: a timeout outside 1..=3600000 is refused before admission.
    let bounded = host(&Store::default(), Arc::new(|_| Ok(String::new())));
    for timeout in [0, 3_600_001] {
        let error = bounded
            .handle(
                "rlm.spawn",
                json!({"prompt": "x", "kwargs": {"name": "x", "timeout_ms": timeout}}),
                "main",
            )
            .unwrap_err();
        assert_eq!(error, "timeout_ms must be an integer between 1 and 3600000");
    }
    assert!(bounded.tasks().unwrap().is_empty());
    bounded.close();
}
