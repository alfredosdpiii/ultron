//! fixtures/fake-tasks.mjs as a `TaskHost`: the scripted task tree, sequential spawned tasks whose outcome depends on
//! their definition, idempotent spawn keys, two lane pins, a settable caller and clock, and a log of task operations.
//! `test.clock`, `test.caller` and `test.finish` are served by [`FakeTasks::inner`].
#![allow(dead_code)]

use kernel::{HostHandler, Signal};
use kernel_apis::TaskHost;
use kernel_apis::util::lock;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

struct State {
    clock: i64,
    caller: Option<String>,
    spawned: usize,
    pins: usize,
    tasks: Vec<Value>,
    keys: Vec<(String, String)>,
    log: Vec<Value>,
}

pub struct FakeTasks {
    state: Mutex<State>,
}

fn done(value: Value) -> Value {
    json!({"status": "succeeded", "value": value, "verification": "unverified"})
}

fn task(id: &str, definition: &str, state: &str, extra: Value) -> Value {
    let mut task = json!({"id": id, "key": format!("key-{id}"), "fingerprint": "f".repeat(64), "definition": definition, "state": state});
    for (key, value) in extra.as_object().unwrap() {
        task[key] = value.clone();
    }
    task
}

impl FakeTasks {
    pub fn new(start: i64) -> Arc<Self> {
        let failed = json!({"status": "failed", "error": "boom", "verification": "unverified"});
        let tasks = vec![
            task("task-a", "worker@1", "running", json!({})),
            task("task-b", "worker@1", "running", json!({"parentId": "task-a"})),
            task("task-c", "worker@1", "running", json!({"parentId": "task-b"})),
            task("task-idle", "worker@1", "admitted", json!({})),
            task(
                "task-done",
                "rlm-child@1",
                "completed",
                json!({"result": done(json!({"answer": 42}))}),
            ),
            task("task-failed", "rlm-child@1", "failed", json!({"result": failed})),
            task("det-done", "det@1", "completed", json!({"result": done(json!(1))})),
            task(
                "child-done",
                "rlm-child@1",
                "completed",
                json!({"parentId": "task-a", "result": done(json!("ok"))}),
            ),
            task(
                "other-done",
                "rlm-child@1",
                "completed",
                json!({"parentId": "task-a", "result": done(json!("ok"))}),
            ),
        ];
        Arc::new(Self {
            state: Mutex::new(State {
                clock: start,
                caller: None,
                spawned: 0,
                pins: 0,
                tasks,
                keys: Vec::new(),
                log: Vec::new(),
            }),
        })
    }

    pub fn log(&self) -> Value {
        Value::Array(lock(&self.state).log.clone())
    }

    /// The fixture host's `test.` module.
    pub fn inner(self: &Arc<Self>) -> Arc<dyn HostHandler> {
        let fake = self.clone();
        Arc::new(move |kind: &str, payload: Map<String, Value>, _: &Signal| {
            let mut state = lock(&fake.state);
            match kind {
                "test.clock" => {
                    state.clock += payload["ms"].as_i64().unwrap();
                    Ok(json!(state.clock))
                }
                "test.caller" => {
                    state.caller = payload["id"].as_str().map(str::to_string);
                    Ok(json!(state.caller))
                }
                "test.finish" => {
                    let value = payload.get("value").cloned().unwrap_or(Value::Null);
                    let found = state.tasks.iter_mut().find(|t| t["id"] == payload["id"]).unwrap();
                    found["state"] = "completed".into();
                    found["result"] = done(value);
                    Ok(json!(true))
                }
                _ => Err(format!("Ultron RLM host request is not wired: {kind}")),
            }
        })
    }
}

impl TaskHost for FakeTasks {
    fn caller_task_id(&self, _lane: &str) -> Option<String> {
        lock(&self.state).caller.clone()
    }

    fn task_lane(&self, task_id: &str) -> Option<String> {
        lock(&self.state).log.push(json!(["taskLane", task_id]));
        (task_id == "task-done").then(|| "lane-task-done".to_string())
    }

    fn strategy(&self, definition: &str) -> Result<String, String> {
        match definition {
            "worker@1" | "rlm-child@1" => Ok("rlm".into()),
            "det@1" => Ok("deterministic".into()),
            _ if definition.starts_with("verify-") => Ok("deterministic".into()),
            _ => Err(format!("Unknown Ultron definition: {definition}")),
        }
    }

    fn tasks(&self) -> Result<Vec<Value>, String> {
        Ok(lock(&self.state).tasks.clone())
    }

    fn spawn(&self, request: &Value, parent: Option<&str>, _signal: &Signal) -> Result<Value, String> {
        let mut state = lock(&self.state);
        state.log.push(json!(["spawn", request, parent]));
        let key = request["key"].as_str().map(str::to_string);
        if let Some(key) = &key
            && let Some((_, id)) = state.keys.iter().find(|(k, _)| k == key)
        {
            let id = id.clone();
            return Ok(state.tasks.iter().find(|t| t["id"] == id.as_str()).cloned().unwrap());
        }
        let definition = request["definition"].as_str().unwrap_or("").to_string();
        if definition == "verify-nostart@1" {
            return Err("Admission refused: budget exhausted".into());
        }
        state.spawned += 1;
        let id = format!("spawned-{}", state.spawned);
        let (task_state, result) = match definition.as_str() {
            "verify-pass@1" => ("completed", Some(done(json!({"passed": true})))),
            "verify-fail@1" => ("completed", Some(done(json!({"passed": false})))),
            "verify-odd@1" => ("completed", Some(done(json!("yes")))),
            "verify-error@1" => (
                "failed",
                Some(json!({"status": "failed", "error": "verifier crashed", "verification": "unverified"})),
            ),
            "verify-noresult@1" => ("completed", None),
            _ => ("running", None),
        };
        let mut extra = json!({});
        if let Some(parent) = parent {
            extra["parentId"] = parent.into();
        }
        if let Some(result) = result {
            extra["result"] = result;
        }
        let mut created = task(&id, &definition, task_state, extra);
        created["key"] = key.clone().unwrap_or_else(|| format!("auto-{}", state.spawned)).into();
        state.tasks.push(created.clone());
        if let Some(key) = key {
            state.keys.push((key, id));
        }
        Ok(created)
    }

    fn result(&self, task_id: &str) -> Result<Value, String> {
        let mut state = lock(&self.state);
        state.log.push(json!(["result", task_id]));
        state
            .tasks
            .iter()
            .find(|t| t["id"] == task_id)
            .and_then(|t| t.get("result").cloned())
            .ok_or_else(|| "result lost".to_string())
    }

    fn cancel(&self, _task_id: &str, _reason: &str) -> Result<Value, String> {
        Err("cancel is not scripted".into())
    }

    fn steer(&self, _task_id: &str, _message: &str, _signal: &Signal) -> Result<bool, String> {
        Ok(false)
    }

    fn now(&self) -> i64 {
        lock(&self.state).clock
    }

    fn pin_lane(&self, lane: &str, holder: &str) -> bool {
        let mut state = lock(&self.state);
        state.log.push(json!(["pinLane", lane, holder]));
        if state.pins >= 2 {
            return false;
        }
        state.pins += 1;
        true
    }

    fn unpin_lane(&self, lane: &str, holder: &str) {
        let mut state = lock(&self.state);
        state.log.push(json!(["unpinLane", lane, holder]));
        state.pins -= 1;
    }
}
