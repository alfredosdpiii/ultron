//! Replays fixtures/task-store.json (recorded by fixtures/generate-task-store.mjs from task-store.ts): the same
//! operations on the Rust journal must return the same tasks and errors and leave the same stored document, byte for
//! byte as JSON text (random task ids are numbered by first appearance on both sides).

use delegation::task_store::{MemoryStore, TaskJournal, TaskStore, persisted_tasks, task_fingerprint};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/task-store.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A store whose n-th write (1-based) or every read fails; the stored value is shared with the test.
struct Scripted {
    value: Arc<Mutex<Option<Value>>>,
    writes: usize,
    fail_write_at: Option<usize>,
    fail_read: bool,
}

impl TaskStore for Scripted {
    fn read(&mut self) -> Result<Option<Value>, String> {
        if self.fail_read {
            return Err("disk".into());
        }
        Ok(self.value.lock().unwrap().clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        self.writes += 1;
        if self.fail_write_at == Some(self.writes) {
            return Err("disk".into());
        }
        *self.value.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

/// Numbers `ultron-task-<uuid>` ids by first appearance, like the generator.
#[derive(Default)]
struct Ids(HashMap<String, String>);

impl Ids {
    fn normalize(&mut self, text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find("ultron-task-") {
            out.push_str(&rest[..at]);
            let id = &rest[at..at + 48];
            let n = self.0.len();
            out.push_str(
                self.0
                    .entry(id.to_string())
                    .or_insert_with(|| format!("ultron-task-#{n}")),
            );
            rest = &rest[at + 48..];
        }
        out.push_str(rest);
        out
    }
}

fn ok(value: Value) -> Value {
    json!({ "status": "succeeded", "value": value, "verification": "unverified" })
}

#[test]
fn delegation_task_journal_replays_typescript_scenarios() {
    let fixture = fixture();
    let mut ids = Ids::default();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let value = Arc::new(Mutex::new(Some(scenario["initial"].clone()).filter(|v| !v.is_null())));
        let mut journal = TaskJournal::new(Box::new(Scripted {
            value: value.clone(),
            writes: 0,
            fail_write_at: scenario["failWriteAt"].as_u64().map(|n| n as usize),
            fail_read: scenario["failRead"] == true,
        }));
        let fingerprint = "a".repeat(64);
        for n in 0..scenario["bulk"].as_u64().unwrap() {
            let (task, _) = journal
                .admit("identity@1", &fingerprint, &format!("request-{n}"), None)
                .unwrap();
            let id = task["id"].as_str().unwrap().to_string();
            journal.transition(&id, "running", None).unwrap();
            if n < scenario["bulk"].as_u64().unwrap() - 1 {
                journal.transition(&id, "completed", Some(ok(json!(n)))).unwrap();
            }
        }
        let mut admitted: Vec<String> = Vec::new();
        for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
            let op = step["op"].as_array().unwrap();
            let arg = |n: usize| -> String {
                match op.get(n).and_then(Value::as_str) {
                    Some(text) if text.starts_with('$') => admitted[text[1..].parse::<usize>().unwrap()].clone(),
                    Some(text) => text.to_string(),
                    None => String::new(),
                }
            };
            let outcome: Result<Value, String> = match op[0].as_str().unwrap() {
                "admit" => {
                    let parent = op.get(4).map(|_| arg(4));
                    journal
                        .admit(&arg(1), &arg(2), &arg(3), parent.as_deref())
                        .map(|(task, created)| {
                            if created {
                                admitted.push(task["id"].as_str().unwrap().to_string());
                            }
                            json!({ "task": task, "created": created })
                        })
                }
                "transition" => journal.transition(&arg(1), &arg(2), op.get(3).cloned()),
                _ => journal.list().map(Value::from),
            };
            let at = format!("{name} step {index} {op:?}");
            match outcome {
                Ok(result) => {
                    assert_eq!(ids.normalize(&result.to_string()), step["result"].to_string(), "{at}");
                }
                Err(error) => assert_eq!(Some(error.as_str()), step["error"].as_str(), "{at}"),
            }
            let stored = value.lock().unwrap().clone().unwrap_or(Value::Null);
            assert_eq!(
                ids.normalize(&stored.to_string()),
                step["stored"].to_string(),
                "{at} stored"
            );
        }
    }
}

#[test]
fn delegation_task_fingerprint_matches_typescript_hashes() {
    for case in fixture()["fingerprints"].as_array().unwrap() {
        assert_eq!(
            task_fingerprint(&case["input"]),
            case["hash"].as_str().unwrap(),
            "{}",
            case["input"]
        );
    }
}

// Ported from ultron-task-store.test.ts.
#[test]
fn delegation_task_fingerprint_is_stable_across_key_order() {
    assert_eq!(
        task_fingerprint(&json!({ "b": 2, "a": [true, null] })),
        task_fingerprint(&json!({ "a": [true, null], "b": 2 }))
    );
    // sha256("{}") as hex: the canonical text of an empty object is hashed, nothing else.
    assert_eq!(
        task_fingerprint(&json!({})),
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
}

// Ported: "the persisted document keeps every unfinished task and only the newest finished ones".
#[test]
fn delegation_persisted_tasks_keep_unfinished_and_newest_finished() {
    let few = vec![
        json!({ "id": "a", "key": "a", "state": "completed" }),
        json!({ "id": "b", "key": "b", "state": "running" }),
    ];
    assert_eq!(persisted_tasks(&few), few);
    let mut journal = TaskJournal::new(Box::new(MemoryStore::default()));
    let fingerprint = "a".repeat(64);
    let mut last = String::new();
    for n in 0..130 {
        let (task, _) = journal
            .admit("identity@1", &fingerprint, &format!("request-{n}"), None)
            .unwrap();
        last = task["id"].as_str().unwrap().to_string();
        journal.transition(&last, "running", None).unwrap();
        if n < 129 {
            journal.transition(&last, "completed", Some(ok(json!(null)))).unwrap();
        }
    }
    let all = journal.list().unwrap();
    assert_eq!(all.len(), 130);
    let kept = persisted_tasks(&all);
    assert_eq!(kept.len(), 101);
    assert_eq!(kept[0]["key"], "request-29");
    assert_eq!(kept[100]["id"], last.as_str());
    assert_eq!(kept[100]["state"], "running");
}
