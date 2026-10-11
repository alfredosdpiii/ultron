//! Replays crates/kernel-apis/fixtures/schedules.json (fixtures/generate-schedules.mjs: the TS schedule module of
//! ultron/schedules.ts behind NativeRlmHost, serving runtime.py through kernel.ts) against `SchedulesModule`, with
//! the same scripted task API, and checks literal cases ported from test/ultron-schedules.test.ts.

mod common;

use common::{FIXED_NOW, Made, assert_area, fixture, inner, run_scenario};
use kernel::Signal;
use kernel_apis::util::lock;
use kernel_apis::{DocumentStore, KernelApis, MemoryDocumentStore, SchedulesModule, TaskHost};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// generate-schedules.mjs `fakeTasks`.
struct FakeTasks {
    clock: Arc<AtomicI64>,
    tasks: Mutex<Vec<Value>>,
    log: Mutex<Vec<Value>>,
}

impl FakeTasks {
    fn new(clock: Arc<AtomicI64>, preloaded: Vec<Value>) -> Self {
        Self {
            clock,
            tasks: Mutex::new(preloaded),
            log: Mutex::new(Vec::new()),
        }
    }
}

impl TaskHost for FakeTasks {
    fn caller_task_id(&self, _lane: &str) -> Option<String> {
        None
    }
    fn task_lane(&self, _task_id: &str) -> Option<String> {
        None
    }
    fn strategy(&self, definition: &str) -> Result<String, String> {
        Err(format!("Unknown Ultron definition: {definition}"))
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        lock(&self.log).push(json!(["tasks"]));
        Ok(lock(&self.tasks).clone())
    }
    fn spawn(&self, request: &Value, parent: Option<&str>, _signal: &Signal) -> Result<Value, String> {
        lock(&self.log).push(json!(["spawn", request, parent]));
        if request["definition"] == "refused-check@1" {
            return Err("Usage budget exhausted".into());
        }
        let mut tasks = lock(&self.tasks);
        if let Some(existing) = tasks.iter().find(|task| task["key"] == request["key"]) {
            return Ok(existing.clone());
        }
        let task = json!({"id": format!("task-{}", tasks.len() + 1), "key": request["key"],
            "definition": request["definition"], "state": "succeeded"});
        tasks.push(task.clone());
        Ok(task)
    }
    fn result(&self, task_id: &str) -> Result<Value, String> {
        lock(&self.log).push(json!(["result", task_id]));
        let tasks = lock(&self.tasks);
        let task = tasks
            .iter()
            .find(|task| task["id"] == task_id)
            .ok_or("Unknown Ultron task")?;
        Ok(match task["definition"].as_str().unwrap_or("") {
            "lost-check@1" => return Err("result unavailable".into()),
            "pass-check@1" => json!({"status": "succeeded", "value": {"passed": true}}),
            "false-check@1" => json!({"status": "succeeded", "value": {"passed": false}}),
            "vague-check@1" => json!({"status": "succeeded", "value": "fine"}),
            "crash-check@1" => json!({"status": "failed", "error": "check crashed"}),
            _ => json!({"status": "succeeded", "value": null}),
        })
    }
    fn cancel(&self, _task_id: &str, _reason: &str) -> Result<Value, String> {
        Err("unused".into())
    }
    fn steer(&self, _task_id: &str, _message: &str, _signal: &Signal) -> Result<bool, String> {
        Ok(false)
    }
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
}

fn host(preloaded: Option<&Value>) -> Made {
    let store = Arc::new(MemoryDocumentStore::new(preloaded.map(|p| p["document"].clone())));
    let clock = Arc::new(AtomicI64::new(FIXED_NOW));
    let tasks = preloaded
        .and_then(|p| p["tasks"].as_array().cloned())
        .unwrap_or_default();
    let fake = Arc::new(FakeTasks::new(clock.clone(), tasks));
    let module = SchedulesModule::new(store.clone(), None);
    let log = fake.clone();
    Made {
        handler: KernelApis::new(inner(clock))
            .with_host(fake, "main")
            .with(Arc::new(module)),
        env: Vec::new(),
        state: Box::new(move || json!({"document": store.read().unwrap(), "log": *lock(&log.log)})),
    }
}

#[test]
fn kernel_apis_schedules_cells_match_typescript_host() {
    assert_area("schedules", &|_: &Path| host(None));
}

#[test]
fn kernel_apis_schedules_restart_resumes_pending_slot_like_typescript() {
    let fixture = fixture("schedules");
    let preloaded = fixture["restart"]["preloaded"].clone();
    for scenario in fixture["restart"]["scenarios"].as_array().unwrap() {
        let actual = run_scenario(scenario, &|_: &Path| host(Some(&preloaded)));
        assert_eq!(actual, *scenario, "{}", scenario["name"]);
    }
}

fn direct(clock: &Arc<AtomicI64>) -> (SchedulesModule, Arc<FakeTasks>, Arc<MemoryDocumentStore>) {
    let store = Arc::new(MemoryDocumentStore::default());
    let fake = Arc::new(FakeTasks::new(clock.clone(), Vec::new()));
    let now = clock.clone();
    let module = SchedulesModule::new(store.clone(), Some(Arc::new(move || now.load(Ordering::SeqCst))));
    (module, fake, store)
}

fn call(module: &SchedulesModule, host: &FakeTasks, kind: &str, payload: Value) -> Result<Value, String> {
    module.handle(kind, payload.as_object().unwrap(), None, host, &Signal::new())
}

/// "duplicate and concurrent ticks in the same slot run the slot once"; a restart on the same stores does not
/// re-run a completed or claimed slot.
#[test]
fn kernel_apis_schedules_slot_runs_once_across_ticks_and_restart() {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let (module, fake, store) = direct(&clock);
    call(
        &module,
        &fake,
        "schedules.create",
        json!({"definition": "identity@1", "input": {"n": 1}, "every_ms": 1000}),
    )
    .unwrap();
    let first = call(&module, &fake, "schedules.tick", json!({})).unwrap();
    assert_eq!(first["fired"].as_array().unwrap().len(), 1);
    assert_eq!(
        call(&module, &fake, "schedules.tick", json!({})).unwrap(),
        json!({"fired": [], "failed": []})
    );
    let restarted = SchedulesModule::new(store.clone(), Some(Arc::new(|| 1_000_500)));
    restarted.start(fake.as_ref()).unwrap();
    assert_eq!(
        call(&restarted, &fake, "schedules.tick", json!({})).unwrap(),
        json!({"fired": [], "failed": []})
    );
    assert_eq!(lock(&fake.tasks).len(), 1);
}

/// "missed ticks coalesce into one run for the latest slot and record the skipped count"; "max_runs completes".
#[test]
fn kernel_apis_schedules_missed_slots_coalesce_and_max_runs_completes() {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let (module, fake, _) = direct(&clock);
    let s = call(
        &module,
        &fake,
        "schedules.create",
        json!({"definition": "a@1", "input": null, "every_ms": 100, "max_runs": 2}),
    )
    .unwrap();
    call(&module, &fake, "schedules.tick", json!({})).unwrap();
    clock.store(1_000_550, Ordering::SeqCst);
    let report = call(&module, &fake, "schedules.tick", json!({})).unwrap();
    assert_eq!(report["fired"][0]["slot"], 5);
    assert_eq!(report["fired"][0]["skipped"], 4);
    let list = call(&module, &fake, "schedules.list", json!({})).unwrap();
    assert_eq!(list[0]["state"], "completed");
    assert_eq!(list[0]["skipped_total"], 4);
    clock.store(1_009_000, Ordering::SeqCst);
    assert_eq!(
        call(&module, &fake, "schedules.tick", json!({})).unwrap()["fired"],
        json!([])
    );
    assert_eq!(
        call(&module, &fake, "schedules.pause", json!({"id": s["id"]})).unwrap_err(),
        "Schedule is completed"
    );
}

/// "admission failure from the usage budget is recorded once per slot without a retry storm".
#[test]
fn kernel_apis_schedules_admission_failure_recorded_once_per_slot() {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let (module, fake, _) = direct(&clock);
    call(
        &module,
        &fake,
        "schedules.create",
        json!({"definition": "refused-check@1", "input": {}, "every_ms": 1000}),
    )
    .unwrap();
    let report = call(&module, &fake, "schedules.tick", json!({})).unwrap();
    assert_eq!(report["failed"][0]["error"], "Usage budget exhausted");
    for _ in 0..3 {
        assert_eq!(
            call(&module, &fake, "schedules.tick", json!({})).unwrap(),
            json!({"fired": [], "failed": []})
        );
    }
    let list = call(&module, &fake, "schedules.list", json!({})).unwrap();
    assert_eq!(list[0]["failures"], 1);
    assert_eq!(
        list[0]["last_failure"],
        json!({"slot": 0, "error": "Usage budget exhausted", "at": 1_000_000})
    );
    assert_eq!(list[0]["pending_slot"], Value::Null);
}

#[test]
fn kernel_apis_schedules_rejects_invalid_documents() {
    let clock = Arc::new(AtomicI64::new(0));
    let fake = FakeTasks::new(clock, Vec::new());
    for document in [
        json!({"version": 2, "schedules": [], "goals": []}),
        json!({"version": 1, "schedules": {}, "goals": []}),
        json!({"version": 1, "schedules": [{"id": "x"}], "goals": [{"id": "x"}]}),
        json!({"version": 1, "schedules": [{"id": 3}], "goals": []}),
    ] {
        let module = SchedulesModule::new(Arc::new(MemoryDocumentStore::new(Some(document.clone()))), None);
        assert!(module.start(&fake).is_err(), "{document}");
    }
    let module = SchedulesModule::new(
        Arc::new(MemoryDocumentStore::new(Some(json!({"schedules": [], "goals": []})))),
        None,
    );
    assert!(module.start(&fake).is_ok());
}
