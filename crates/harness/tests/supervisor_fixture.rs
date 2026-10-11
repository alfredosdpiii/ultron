//! Replays crates/harness/fixtures/task-supervisor.json, recorded from the TypeScript task host
//! (packages/coding-agent/src/ultron/rlm/native-host.ts) by fixtures/generate-supervisor.mjs with controlled lanes.
//! Every step's value or error message, the stored journal document, and each task's abort count must match.
//!
//! The lane layer is replay-side: a task's lane exists in the owner that started it, so a call "from" a task of
//! an earlier owner is a root-lane call, as in TypeScript. What a finished or failed lane run produces (the
//! child's verdict-checked result) is out of this crate's scope: the replay reports the recorded result.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use harness::supervisor::{SpawnRequest, SupervisorError, TaskSupervisor};
use harness::task_store::{TaskDocumentStore, TaskJournal};
use serde_json::{Map, Value, json};

#[derive(Clone, Default)]
struct MemoryStore(Arc<Mutex<Option<Value>>>);

impl TaskDocumentStore for MemoryStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn write(&mut self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

type Queue = Arc<Mutex<VecDeque<String>>>;
type Counts = Arc<Mutex<HashMap<String, u64>>>;

fn queue(scenario: &Value, field: &str, keep: impl Fn(&str) -> bool) -> Queue {
    let items = scenario[field].as_array().unwrap().iter().map(|v| v.as_str().unwrap());
    Arc::new(Mutex::new(items.filter(|s| keep(s)).map(str::to_owned).collect()))
}

fn open(store: &MemoryStore, ids: &Queue, keys: &Queue) -> (TaskSupervisor<MemoryStore>, Counts) {
    let (ids, keys) = (ids.clone(), keys.clone());
    let journal = TaskJournal::with_id_source(
        store.clone(),
        Box::new(move || ids.lock().unwrap().pop_front().expect("recorded id")),
    );
    let mut supervisor = TaskSupervisor::with_journal(
        journal,
        Box::new(move || keys.lock().unwrap().pop_front().expect("recorded key")),
    );
    let counts = Counts::default();
    let sink = counts.clone();
    supervisor.on_abort(Box::new(move |id| {
        *sink.lock().unwrap().entry(id.into()).or_default() += 1
    }));
    (supervisor, counts)
}

fn outcome(result: Result<Value, SupervisorError>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": error.to_string() }),
    }
}

fn spawn_payload(step: &Value) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("definition".into(), "rlm-child@1".into());
    payload.insert("input".into(), json!({ "prompt": step["prompt"] }));
    for field in ["key", "model", "timeout_ms"] {
        if let Some(value) = step.get(field) {
            payload.insert(field.into(), value.clone());
        }
    }
    if let Some(set) = step.get("set").and_then(Value::as_object) {
        for (field, value) in set {
            payload.insert(field.clone(), value.clone());
        }
    }
    payload
}

fn replay(name: &str, scenario: &Value) {
    let store = MemoryStore::default();
    let ids = queue(scenario, "ids", |_| true);
    // Scenario-chosen keys are stored as given; only generated ones are minted.
    let keys = queue(scenario, "keys", |k| k.len() == 36);
    let (mut host, mut counts) = open(&store, &ids, &keys);
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut lanes: HashSet<String> = HashSet::new();
    for (n, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        let label = |field: &str| labels.get(step[field].as_str().unwrap()).cloned().unwrap();
        let caller = step.get("from").map(|_| label("from")).filter(|id| lanes.contains(id));
        let caller = caller.as_deref();
        let got = match step["op"].as_str().unwrap() {
            "spawn" => {
                let spawned = SpawnRequest::from_payload(&spawn_payload(step))
                    .map_err(|e| json!({ "error": e }))
                    .and_then(|request| match host.spawn(caller, &request) {
                        Ok(value) => Ok(value),
                        Err(error) => Err(json!({ "error": error.to_string() })),
                    });
                match spawned {
                    Ok(value) => {
                        let id = value["id"].as_str().unwrap().to_owned();
                        if value["state"] == "admitted" {
                            lanes.insert(id.clone());
                        }
                        labels.insert(step["as"].as_str().unwrap().into(), id);
                        json!({ "ok": value })
                    }
                    Err(error) => error,
                }
            }
            "finish" | "fail" => outcome(host.complete(&label("task"), step["outcome"]["ok"].clone())),
            "cancel" => outcome(
                host.cancel(caller, &label("task"))
                    .map(|cancelled| json!({ "cancelled": cancelled })),
            ),
            "inspect" => outcome(host.inspect(caller, &label("task"))),
            "result" => outcome(host.result(caller, &label("task")).map(|r| r.expect("a finished task"))),
            "status" => outcome(host.status(caller).map(Value::from)),
            "close" => outcome(host.close().map(|()| Value::Null)),
            "reopen" | "crash" => {
                if step["op"] == "reopen" {
                    host.close().unwrap();
                }
                (host, counts) = open(&store, &ids, &keys);
                lanes.clear();
                json!({ "ok": null })
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(got, step["outcome"], "{name} step {n} ({}) outcome", step["op"]);
        let stored = store.0.lock().unwrap().clone().unwrap_or(Value::Null);
        assert_eq!(stored, step["stored"], "{name} step {n} ({}) stored", step["op"]);
        let counts = counts.lock().unwrap();
        for (task, expected) in step["aborts"].as_object().unwrap() {
            let got = counts.get(&labels[task]).copied().unwrap_or(0);
            assert_eq!(
                json!(got),
                *expected,
                "{name} step {n} ({}) aborts of {task}",
                step["op"]
            );
        }
    }
}

#[test]
fn harness_supervisor_replays_typescript_host() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/task-supervisor.json")).unwrap();
    let scenarios = fixture["scenarios"].as_object().unwrap();
    assert!(scenarios.len() >= 7);
    for (name, scenario) in scenarios {
        replay(name, scenario);
    }
}
