//! Replays fixtures/gates.json (recorded by fixtures/generate-gates.mjs from `createReleaseGateModule` of
//! release-gate.ts): every `gates.*` request, served through the module's `ApiModule` face with the caller task and
//! clock of the request's host, must return the same result or error and leave the same stored document (as JSON
//! text) after every step, across reopens, tampered, newer and corrupt documents and failing writes.

use kernel::Signal;
use kernel_apis::util::stringify;
use kernel_apis::{ApiModule, DocumentStore, Request, TaskHost};
use serde_json::{Value, json};
use services::ReleaseGateModule;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/gates.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// `createMemoryModuleStore` whose writes can be made to fail.
#[derive(Default)]
struct Store {
    document: Mutex<Option<Value>>,
    fail: AtomicBool,
}

impl DocumentStore for Store {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(self.document.lock().unwrap().clone())
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("disk full".into());
        }
        *self.document.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

/// The host a request comes through: only its clock is used.
struct Clocked(Arc<AtomicI64>);

impl TaskHost for Clocked {
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
        Ok(Vec::new())
    }
    fn spawn(&self, _: &Value, _: Option<&str>, _: &Signal) -> Result<Value, String> {
        Err("no tasks".into())
    }
    fn result(&self, _: &str) -> Result<Value, String> {
        Err("no tasks".into())
    }
    fn cancel(&self, _: &str, _: &str) -> Result<Value, String> {
        Err("no tasks".into())
    }
    fn steer(&self, _: &str, _: &str, _: &Signal) -> Result<bool, String> {
        Ok(false)
    }
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[test]
fn services_release_gates_match_the_release_gate_module() {
    let mut requests = 0;
    for session in fixture().as_array().unwrap() {
        let name = session["name"].as_str().unwrap();
        let store = Arc::new(Store::default());
        let ids = Arc::new(AtomicU64::new(0));
        let open = || {
            let ids = ids.clone();
            ReleaseGateModule::new(store.clone(), None).with_ids(Arc::new(move || {
                format!("00000000-0000-4000-8000-{:012}", ids.fetch_add(1, Ordering::SeqCst) + 1)
            }))
        };
        let clock = Arc::new(AtomicI64::new(5000));
        let host = Clocked(clock.clone());
        let mut module = open();
        let mut started: Option<Result<(), String>> = None;
        let mut caller: Option<String> = None;
        for (index, step) in session["steps"].as_array().unwrap().iter().enumerate() {
            let args = step["step"].as_array().unwrap();
            let kind = args[0].as_str().unwrap();
            match kind {
                "reopen" => {
                    module = open();
                    started = None;
                }
                "caller" => caller = args[1].as_str().map(str::to_string),
                "at" => clock.store(args[1].as_i64().unwrap(), Ordering::SeqCst),
                "seed" => *store.document.lock().unwrap() = Some(args[1].clone()),
                "tamper" => {
                    let mut saved = store.document.lock().unwrap().clone().unwrap();
                    for check in saved["gates"][0]["checks"].as_array_mut().unwrap() {
                        check["required"] = json!(false);
                    }
                    *store.document.lock().unwrap() = Some(saved);
                }
                "failWrites" => store.fail.store(args[1] == true, Ordering::SeqCst),
                _ => {
                    requests += 1;
                    let start = started.get_or_insert_with(|| module.start(&host)).clone();
                    let payload = args[1].as_object().cloned().unwrap_or_default();
                    let outcome = start.and_then(|()| {
                        module.handle(&Request {
                            kind,
                            payload: &payload,
                            lane: "lane",
                            caller_task: caller.as_deref(),
                            host: &host,
                            signal: &Signal::new(),
                        })
                    });
                    let at = format!("{name} / step {index} {kind} {}", stringify(&args[1]));
                    match (step.get("ok"), step.get("error"), outcome) {
                        (Some(want), _, Ok(got)) => assert_eq!(stringify(&got), stringify(want), "{at}"),
                        (_, Some(want), Err(got)) => assert_eq!(got, want.as_str().unwrap(), "{at}"),
                        (want, error, got) => panic!("{at}: want {want:?} / {error:?}, got {got:?}"),
                    }
                    let document = store.document.lock().unwrap().clone().unwrap_or(Value::Null);
                    assert_eq!(
                        stringify(&document),
                        step["document"].as_str().unwrap(),
                        "{at}: document"
                    );
                }
            }
        }
    }
    assert_eq!(requests, 78);
}
