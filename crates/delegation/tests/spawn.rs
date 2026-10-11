//! Replays fixtures/spawn.json (recorded by fixtures/generate-spawn.mjs from NativeRlmHost with scripted
//! `externalChild` runs): the same host calls from the root and from each child's lane return the same results and
//! errors, children run with the same inputs, the same completion events fire (compared by task, since TS orders
//! concurrent ends by microtask), and the journal ends the same. Ids and keys are numbered by first appearance.

use delegation::host::{ChildRunner, Delegation, DelegationOptions, DetachedEnd};
use delegation::task_store::TaskStore;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Shared {
    value: Option<Value>,
}

struct Store(Arc<Mutex<Shared>>);

impl TaskStore for Store {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().value.clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        self.0.lock().unwrap().value = Some(document.clone());
        Ok(())
    }
}

/// Numbers random task ids and idempotency keys by first appearance, like the generator.
#[derive(Default)]
struct Names {
    ids: HashMap<String, String>,
    keys: HashMap<String, String>,
}

impl Names {
    fn id(&mut self, raw: &str) -> String {
        let n = self.ids.len();
        self.ids
            .entry(raw.to_string())
            .or_insert_with(|| format!("ultron-task-#{n}"))
            .clone()
    }
    fn normalize(&mut self, value: &Value) -> Value {
        // Walk the text in order, so numbering follows textual first appearance like the generator's regex.
        let mut text = serde_json::to_string_pretty(value).unwrap();
        let mut out = String::new();
        while let Some(at) = text.find("ultron-task-") {
            let end = at + 48;
            if text.len() >= end && text.as_bytes()[at + 12] != b'#' {
                out.push_str(&text[..at]);
                let id = text[at..end].to_string();
                out.push_str(&self.id(&id));
                text = text[end..].to_string();
            } else {
                out.push_str(&text[..at + 12]);
                text = text[at + 12..].to_string();
            }
        }
        out.push_str(&text);
        let mut value: Value = serde_json::from_str(&out).unwrap();
        self.keys_in(&mut value);
        value
    }
    /// Keys are numbered by the order TS emitted events, which follows microtask order for concurrent ends; they
    /// are checked per task (one key, the same everywhere) and left out of the comparison.
    fn keys_in(&mut self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                if let (Some(Value::String(id)), Some(Value::String(key))) = (map.get("id"), map.get("key")) {
                    let previous = self.keys.entry(id.clone()).or_insert_with(|| key.clone());
                    assert_eq!(previous, key, "task {id} has one idempotency key");
                    map.remove("key");
                }
                map.values_mut().for_each(|v| self.keys_in(v));
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.keys_in(v)),
            _ => {}
        }
    }
}

#[derive(Default)]
struct Children {
    /// Per child index: whether it waits at its gate.
    parked: HashMap<usize, bool>,
    open: Vec<bool>,
    runs: Vec<Value>,
}

fn settle(host: &Delegation, children: &Arc<(Mutex<Children>, Condvar)>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let tasks = host.tasks().unwrap();
        let state = children.0.lock().unwrap();
        // Every child has ended or waits at its gate (its record pushed, so its runner has started).
        let done = tasks
            .iter()
            .enumerate()
            .all(|(index, task)| task["state"] != "running" || state.parked.get(&index).copied().unwrap_or(false));
        if done {
            return;
        }
        assert!(Instant::now() < deadline, "children did not settle");
        drop(state);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn delegation_spawn_collect_finish_replay_typescript_scenarios() {
    let strip = |mut value: Value| {
        Names::default().keys_in(&mut value);
        value
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/spawn.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut names = Names::default();
    for scenario in fixture.as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let scripts = scenario["children"].as_array().unwrap().clone();
        let children = Arc::new((
            Mutex::new(Children {
                open: vec![false; scripts.len()],
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let order: Arc<Mutex<Vec<String>>> = Arc::default();
        let runner: ChildRunner = {
            let (children, order) = (children.clone(), order.clone());
            Arc::new(move |run| {
                // Scripts follow admission order (TS starts runs in that order; Rust threads race).
                let index = run
                    .host
                    .tasks()
                    .unwrap()
                    .iter()
                    .position(|t| t["id"] == run.task_id.as_str())
                    .unwrap();
                order.lock().unwrap().push(run.task_id.clone());
                let script = &scripts[index];
                let gate = |children: &Arc<(Mutex<Children>, Condvar)>| {
                    let (lock, cvar) = &**children;
                    let mut state = lock.lock().unwrap();
                    state.parked.insert(index, true);
                    while !state.open[index] {
                        state = cvar.wait(state).unwrap();
                    }
                };
                if script["gate"] == "before" {
                    gate(&children);
                }
                let mut calls = Vec::new();
                for call in script["calls"].as_array().unwrap() {
                    let kind = call[0].as_str().unwrap();
                    calls.push(match run.host.handle(kind, call[1].clone(), &run.lane_name) {
                        Ok(result) => json!({ "type": kind, "result": result }),
                        Err(error) => json!({ "type": kind, "error": error }),
                    });
                }
                let record = json!({ "index": index, "taskId": run.task_id, "laneName": run.lane_name,
                    "prompt": run.prompt, "level": run.level, "allowance": run.allowance, "model": run.model,
                    "timeoutMs": run.timeout_ms, "calls": calls });
                children.0.lock().unwrap().runs.push(record);
                if script["gate"] == true {
                    gate(&children);
                }
                match script["throws"].as_str() {
                    Some(error) => Err(error.to_string()),
                    None => Ok(script["text"].as_str().unwrap().to_string()),
                }
            })
        };
        let spawn_depth = delegation::host::spawn_depth_limit(scenario["env"]["ULTRON_SPAWN_DEPTH"].as_str());
        let (host, events) = Delegation::new(DelegationOptions {
            store: Box::new(Store(shared.clone())),
            collect: delegation::host::CollectMode::Wait,
            runner,
            spawn_depth,
            child_model: None,
            workspace: None,
            session_id: "session".into(),
            worktree_setup: None,
            keep_worktrees: false,
            now: Box::new(|| 1000.0),
        });
        let host_ref = host.clone();
        let mut ids: Vec<String> = Vec::new();
        let mut pending: Vec<std::thread::JoinHandle<Result<Value, String>>> = Vec::new();
        let resolve = |payload: &Value, ids: &[String]| -> Value {
            let mut text = payload.to_string();
            for (n, id) in ids.iter().enumerate().rev() {
                text = text.replace(&format!("\"${n}\""), &format!("\"{id}\""));
            }
            serde_json::from_str(&text).unwrap()
        };
        let steps = scenario["steps"].as_array().unwrap();
        let check = |step: &Value, outcome: Result<Value, String>, names: &mut Names, ids: &mut Vec<String>| {
            let at = format!("{name} step {}", step["op"]);
            match outcome {
                Ok(result) => {
                    if step["op"][1] == "rlm.spawn" {
                        ids.push(result["rlm_child_id"].as_str().unwrap().to_string());
                    }
                    assert_eq!(names.normalize(&result), strip(step["result"].clone()), "{at}");
                }
                Err(error) => assert_eq!(Some(error.as_str()), step["error"].as_str(), "{at}"),
            }
        };
        for step in steps {
            let op = step["op"].as_array().unwrap();
            match op[0].as_str().unwrap() {
                "call" => {
                    let outcome = host.handle(op[1].as_str().unwrap(), resolve(&op[2], &ids), "main");
                    check(step, outcome, &mut names, &mut ids);
                }
                "async" => {
                    let (host, kind, payload) =
                        (host.clone(), op[1].as_str().unwrap().to_string(), resolve(&op[2], &ids));
                    let watched = payload["selectors"][0].as_str().unwrap().to_string();
                    pending.push(std::thread::spawn(move || host.handle(&kind, payload, "main")));
                    let host = host_ref.clone();
                    while host.waiters(&watched) == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                "join" => {
                    let outcome = pending.remove(0).join().unwrap();
                    check(step, outcome, &mut names, &mut ids);
                }
                "release" => {
                    let index = op[1].as_u64().unwrap() as usize;
                    let mut state = children.0.lock().unwrap();
                    state.open[index] = true;
                    state.parked.insert(index, false);
                    children.1.notify_all();
                }
                "sleep" => std::thread::sleep(Duration::from_millis(op[1].as_u64().unwrap())),
                _ => settle(&host, &children),
            }
        }
        {
            let mut state = children.0.lock().unwrap();
            state.open.iter_mut().for_each(|o| *o = true);
            children.1.notify_all();
        }
        settle(&host, &children);
        host.close();

        let mut runs = children.0.lock().unwrap().runs.clone();
        runs.sort_by_key(|r| r["index"].as_u64());
        let (got_runs, want_runs) = (names.normalize(&Value::from(runs)), strip(scenario["runs"].clone()));
        for (g, w) in got_runs.as_array().unwrap().iter().zip(want_runs.as_array().unwrap()) {
            assert_eq!(g, w, "{name} run {}", w["index"]);
        }
        assert_eq!(got_runs, want_runs, "{name} runs");
        let mut got: Vec<Value> = events
            .try_iter()
            .map(|e: DetachedEnd| {
                json!({ "task": e.task, "kind": e.kind, "ownerLane": e.owner_lane, "awaited": e.awaited, "fetch": e.fetch })
            })
            .collect();
        let mut want = strip(scenario["events"].clone()).as_array().unwrap().clone();
        got = got.iter().map(|e| names.normalize(e)).collect();
        let by_id = |e: &Value| e["task"]["id"].as_str().unwrap().to_string();
        got.sort_by_key(by_id);
        want.sort_by_key(by_id);
        assert_eq!(Value::from(got), Value::from(want), "{name} events");
        let journal = shared.lock().unwrap().value.clone().unwrap_or(Value::Null);
        assert_eq!(
            names.normalize(&journal),
            strip(scenario["journal"].clone()),
            "{name} journal"
        );
    }
}
