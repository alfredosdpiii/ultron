//! The task side of the TypeScript `NativeRlmHost` as these acceptance tests use it, over the real Rust task runtime:
//! admission, idempotency keys, parent links, cancellation cascades, abort hooks and restart recovery are the
//! `harness::supervisor::TaskSupervisor` over its task journal; the kernel-API modules are the real
//! `kernel_apis` modules, started once and dispatched by prefix as the TS host does.
//!
//! What the TS tests fake, this fakes the same way: task lanes are controlled probes (a prompt that blocks until the
//! test finishes it, a steer log, an abort count), named `ultron.<definition id>.<task id>` like the TS host's lanes.
//! The `agents.*` / `background.*` requests the TS tests send to the host map onto the supervisor's operations.
#![allow(dead_code)]

use harness::supervisor::{SpawnRequest, TaskSupervisor};
use harness::task_store::TaskDocumentStore;
use kernel::Signal;
use kernel_apis::{ApiModule, Request, TaskHost};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The session value holding the task journal (`memoryStore()` of the TS tests); clones share it.
#[derive(Clone, Default)]
pub struct JournalStore(pub Arc<Mutex<Option<Value>>>);

impl TaskDocumentStore for JournalStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(lock(&self.0).clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        *lock(&self.0) = Some(document.clone());
        Ok(())
    }
}

/// A task lane as the TS `controlledHarness` / `waitingLane` fakes it.
#[derive(Default, Clone, Debug)]
pub struct Probe {
    pub steered: Vec<String>,
    pub aborts: usize,
}

pub struct Tasks {
    supervisor: Mutex<TaskSupervisor<JournalStore>>,
    store: JournalStore,
    /// lane → task id, for tasks this owner started.
    lanes: Mutex<HashMap<String, String>>,
    probes: Arc<Mutex<HashMap<String, Probe>>>,
    pub clock: Arc<AtomicI64>,
}

pub fn lane_name(definition: &str, id: &str) -> String {
    format!("ultron.{}.{id}", definition.split('@').next().unwrap_or(definition))
}

impl Tasks {
    pub fn new(store: JournalStore, clock: Arc<AtomicI64>) -> Arc<Tasks> {
        let probes: Arc<Mutex<HashMap<String, Probe>>> = Arc::default();
        let mut supervisor = TaskSupervisor::new(store.clone());
        let aborted = probes.clone();
        supervisor.on_abort(Box::new(move |id: &str| {
            lock(&aborted).entry(id.to_string()).or_default().aborts += 1;
        }));
        Arc::new(Tasks {
            supervisor: Mutex::new(supervisor),
            store,
            lanes: Mutex::default(),
            probes,
            clock,
        })
    }

    pub fn probe(&self, id: &str) -> Probe {
        lock(&self.probes).get(id).cloned().unwrap_or_default()
    }

    /// The journal document's tasks (`host.api.tasks()`), after the owner has loaded (and recovered) it.
    pub fn journal(&self) -> Vec<Value> {
        let _ = lock(&self.supervisor).status(None);
        lock(&self.store.0)
            .as_ref()
            .and_then(|doc| doc["tasks"].as_array().cloned())
            .unwrap_or_default()
    }

    fn task(&self, id: &str) -> Option<Value> {
        self.journal().into_iter().find(|task| task["id"] == id)
    }

    /// Admit and start a task whose lane then waits (its prompt is pending).
    pub fn start(
        &self,
        definition: &str,
        input: Value,
        key: Option<String>,
        parent: Option<&str>,
    ) -> Result<Value, String> {
        let request = SpawnRequest {
            definition: definition.into(),
            input,
            model: None,
            key,
            timeout_ms: harness::supervisor::DEFAULT_TIMEOUT_MS,
        };
        let admitted = lock(&self.supervisor)
            .spawn(parent, &request)
            .map_err(|e| e.to_string())?;
        let id = admitted["id"].as_str().unwrap_or_default().to_string();
        lock(&self.lanes).insert(lane_name(definition, &id), id.clone());
        lock(&self.probes).entry(id.clone()).or_default();
        self.task(&id).ok_or_else(|| "Unknown Ultron task".into())
    }

    /// The lane's prompt returned `text` (the TS probe's `finish`): the task completes with it.
    pub fn finish(&self, id: &str, text: &str) -> Value {
        lock(&self.supervisor)
            .complete(
                id,
                json!({"status": "succeeded", "value": text, "verification": "unverified"}),
            )
            .unwrap()
    }

    pub fn close(&self) {
        let _ = lock(&self.supervisor).close();
    }
}

impl TaskHost for Tasks {
    fn caller_task_id(&self, lane: &str) -> Option<String> {
        lock(&self.lanes).get(lane).cloned()
    }
    fn task_lane(&self, task_id: &str) -> Option<String> {
        lock(&self.lanes)
            .iter()
            .find(|(_, id)| id.as_str() == task_id)
            .map(|(lane, _)| lane.clone())
    }
    fn strategy(&self, definition: &str) -> Result<String, String> {
        match definition {
            "rlm-child@1" | "background-job@1" => Ok("rlm".into()),
            "identity@1" => Ok("deterministic".into()),
            _ => Err(format!("Unknown Ultron definition: {definition}")),
        }
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        Ok(self.journal())
    }
    fn spawn(&self, request: &Value, parent: Option<&str>, _signal: &Signal) -> Result<Value, String> {
        let definition = request["definition"].as_str().unwrap_or_default();
        self.strategy(definition)?;
        self.start(
            definition,
            request.get("input").cloned().unwrap_or(Value::Null),
            request["key"].as_str().map(str::to_string),
            parent,
        )
    }
    fn result(&self, task_id: &str) -> Result<Value, String> {
        lock(&self.supervisor)
            .result(None, task_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("task {task_id} is still running"))
    }
    fn cancel(&self, task_id: &str, _reason: &str) -> Result<Value, String> {
        let cancelled = lock(&self.supervisor)
            .cancel(None, task_id)
            .map_err(|e| e.to_string())?;
        Ok(json!({"cancelled": cancelled}))
    }
    fn steer(&self, task_id: &str, message: &str, _signal: &Signal) -> Result<bool, String> {
        // A live lane is one whose task has not ended.
        if self.task(task_id).is_none_or(|task| task.get("result").is_some()) {
            return Ok(false);
        }
        if !lock(&self.lanes).values().any(|id| id == task_id) {
            return Ok(false);
        }
        lock(&self.probes)
            .entry(task_id.to_string())
            .or_default()
            .steered
            .push(message.to_string());
        Ok(true)
    }
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
}

/// `NativeRlmHost.handle(type, payload, context, {lane})`: host requests, then modules by prefix.
pub struct Host {
    pub tasks: Arc<Tasks>,
    modules: Vec<Arc<dyn ApiModule>>,
    started: Mutex<Option<Result<(), String>>>,
}

fn fields(payload: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match payload.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown payload field: {key}")),
        None => Ok(()),
    }
}

impl Host {
    pub fn new(tasks: Arc<Tasks>, modules: Vec<Arc<dyn ApiModule>>) -> Host {
        Host {
            tasks,
            modules,
            started: Mutex::new(None),
        }
    }

    /// From the root lane ("main").
    pub fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.call_in("main", kind, payload)
    }

    pub fn call_in(&self, lane: &str, kind: &str, payload: Value) -> Result<Value, String> {
        let payload = payload.as_object().cloned().unwrap_or_default();
        let tasks = &self.tasks;
        let caller = tasks.caller_task_id(lane);
        let id = || {
            payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let supervisor = || lock(&tasks.supervisor);
        match kind {
            "background.start" => {
                fields(&payload, &["prompt", "model", "key", "timeout_ms"])?;
                let prompt = payload["prompt"].as_str().ok_or("prompt must be a nonempty string")?;
                return tasks.start("background-job@1", json!({"prompt": prompt}), None, caller.as_deref());
            }
            "agents.spawn" => {
                fields(&payload, &["definition", "input", "model", "key", "timeout_ms"])?;
                let definition = payload["definition"].as_str().unwrap_or_default();
                tasks.strategy(definition)?;
                return tasks.start(definition, payload["input"].clone(), None, caller.as_deref());
            }
            "agents.inspect" => {
                return supervisor()
                    .inspect(caller.as_deref(), &id())
                    .map_err(|e| e.to_string());
            }
            "agents.status" => {
                let listed = supervisor().status(caller.as_deref()).map_err(|e| e.to_string())?;
                return Ok(json!({"tasks": listed}));
            }
            "agents.cancel" => {
                let cancelled = supervisor()
                    .cancel(caller.as_deref(), &id())
                    .map_err(|e| e.to_string())?;
                return Ok(json!({"cancelled": cancelled}));
            }
            "agents.result" => {
                return supervisor()
                    .result(caller.as_deref(), &id())
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "task is still running".into());
            }
            _ => {}
        }
        lock(&self.started)
            .get_or_insert_with(|| self.modules.iter().try_for_each(|m| m.start(tasks.as_ref())))
            .clone()?;
        let module = self
            .modules
            .iter()
            .find(|m| m.prefixes().iter().any(|prefix| kind.starts_with(prefix)))
            .ok_or_else(|| format!("Ultron RLM host request is not wired: {kind}"))?;
        let signal = Signal::new();
        module.handle(&Request {
            kind,
            payload: &payload,
            lane,
            caller_task: caller.as_deref(),
            host: tasks.as_ref(),
            signal: &signal,
        })
    }
}

/// `expect(promise).rejects.toThrow(text)`.
pub fn rejects(result: Result<Value, String>, text: &str) {
    match result {
        Ok(value) => panic!("expected an error containing {text:?}, got {value}"),
        Err(error) => assert!(error.contains(text), "{error:?} does not contain {text:?}"),
    }
}
