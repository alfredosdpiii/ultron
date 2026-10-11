//! Replays crates/harness/fixtures/task-store.json, recorded from the TypeScript task journal
//! (packages/coding-agent/src/ultron/rlm/task-store.ts) by fixtures/generate.mjs: every operation's value or
//! error message and the stored document after it must match.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use harness::task_store::{TaskDocumentStore, TaskJournal, TaskState, task_fingerprint};
use serde_json::{Value, json};

#[derive(Default)]
struct Shared {
    value: Option<Value>,
    fail_writes: bool,
    fail_read: bool,
}

#[derive(Clone, Default)]
struct MemoryStore(Arc<Mutex<Shared>>);

impl TaskDocumentStore for MemoryStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        let shared = self.0.lock().unwrap();
        if shared.fail_read {
            return Err("read failed".into());
        }
        Ok(shared.value.clone())
    }

    fn write(&mut self, document: &Value) -> Result<(), String> {
        let mut shared = self.0.lock().unwrap();
        if shared.fail_writes {
            return Err("write failed".into());
        }
        shared.value = Some(document.clone());
        Ok(())
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/task-store.json")).unwrap()
}

fn open(store: &MemoryStore, ids: &Arc<Mutex<VecDeque<String>>>) -> TaskJournal<MemoryStore> {
    let ids = ids.clone();
    TaskJournal::with_id_source(
        store.clone(),
        Box::new(move || ids.lock().unwrap().pop_front().expect("recorded id")),
    )
}

fn outcome<T>(result: Result<T, harness::task_store::TaskStoreError>, ok: impl FnOnce(T) -> Value) -> Value {
    match result {
        Ok(value) => json!({ "ok": ok(value) }),
        Err(error) => json!({ "error": error.to_string() }),
    }
}

fn replay(name: &str, scenario: &Value) {
    let minted: Vec<String> = scenario["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().into())
        .collect();
    let ids = Arc::new(Mutex::new(minted.iter().cloned().collect::<VecDeque<_>>()));
    let resolve = |v: &Value| -> String {
        let text = v.as_str().unwrap();
        match text.strip_prefix('$') {
            Some(n) => minted[n.parse::<usize>().unwrap()].clone(),
            None => text.into(),
        }
    };
    let store = MemoryStore::default();
    let mut journal = open(&store, &ids);
    for (n, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        let got = match step["op"].as_str().unwrap() {
            "seed" => {
                store.0.lock().unwrap().value = Some(step["value"].clone());
                json!({ "ok": null })
            }
            "open" => {
                journal = open(&store, &ids);
                json!({ "ok": null })
            }
            "failWrites" => {
                store.0.lock().unwrap().fail_writes = step["on"].as_bool().unwrap();
                json!({ "ok": null })
            }
            "failRead" => {
                store.0.lock().unwrap().fail_read = step["on"].as_bool().unwrap();
                json!({ "ok": null })
            }
            "ready" => outcome(journal.ready(), |()| Value::Null),
            "list" => outcome(journal.list(), |tasks| tasks.iter().map(|t| t.to_json()).collect()),
            "count" => outcome(journal.list(), |tasks| json!(tasks.len())),
            "admit" => {
                let parent = step.get("parentId").map(resolve);
                let result = journal.admit(
                    step["definition"].as_str().unwrap(),
                    step["fingerprint"].as_str().unwrap(),
                    step["key"].as_str().unwrap(),
                    parent.as_deref(),
                );
                outcome(
                    result,
                    |(task, created)| json!({ "task": task.to_json(), "created": created }),
                )
            }
            "transition" => {
                let state = TaskState::parse(step["state"].as_str().unwrap()).unwrap();
                let result = journal.transition(&resolve(&step["id"]), state, step.get("result").cloned());
                outcome(result, |task| task.to_json())
            }
            "cycle" => {
                for k in 0..step["count"].as_u64().unwrap() {
                    let (task, _) = journal
                        .admit("identity@1", &"a".repeat(64), &format!("request-{k}"), None)
                        .unwrap();
                    journal.transition(&task.id, TaskState::Running, None).unwrap();
                    if k + 1 < step["count"].as_u64().unwrap() {
                        let done = json!({ "status": "succeeded", "verification": "unverified" });
                        journal.transition(&task.id, TaskState::Completed, Some(done)).unwrap();
                    }
                }
                json!({ "ok": null })
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, step["outcome"], "{name} step {n} ({}) outcome", step["op"]);
        let stored = store.0.lock().unwrap().value.clone().unwrap_or(Value::Null);
        assert_eq!(
            stored, step["stored"],
            "{name} step {n} ({}) stored document",
            step["op"]
        );
    }
}

#[test]
fn harness_task_store_replays_typescript_scenarios() {
    let fixture = fixture();
    let scenarios = fixture["scenarios"].as_object().unwrap();
    assert!(scenarios.len() >= 30);
    for (name, scenario) in scenarios {
        replay(name, scenario);
    }
}

#[test]
fn harness_task_store_fingerprints_match_typescript() {
    for case in fixture()["fingerprints"].as_array().unwrap() {
        assert_eq!(
            task_fingerprint(&case["input"]),
            case["hash"].as_str().unwrap(),
            "{}",
            case["input"]
        );
    }
}
