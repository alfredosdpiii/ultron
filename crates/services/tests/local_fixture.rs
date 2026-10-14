//! Replays fixtures/local.json (recorded by fixtures/generate-local.mjs from `NativeLocalServices` of
//! local-services.ts): every `refinements.*` and `experiments.*` request must return the same result or error and
//! leave the same documents (as JSON text, key order included) after every step, across reopens with other options,
//! conversation branches, seeded (legacy, newer, corrupt) documents and failing writes.

use kernel_apis::artifacts::KeyedDocuments;
use kernel_apis::util::stringify;
use serde_json::{Map, Value, json};
use services::local::{Branch, LocalOptions, LocalServices, Validator};
use services::validate_refinement_content;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/local.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A JavaScript `Map` shared with the test, whose writes can be made to fail.
#[derive(Clone, Default)]
struct Documents {
    values: Arc<Mutex<Map<String, Value>>>,
    fail: Arc<AtomicBool>,
}

impl KeyedDocuments for Documents {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        Ok(self.values.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("disk full".into());
        }
        self.values.lock().unwrap().insert(key.into(), value.clone());
        Ok(())
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        Ok(self
            .values
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
}

/// The generator's validators.
fn validator(name: &str) -> Option<Validator> {
    match name {
        "worker" => Some(Arc::new(validate_refinement_content)),
        "procedure" => Some(Arc::new(|kind: &str, content: &Value| {
            if kind != "skill" {
                return Ok(true);
            }
            let Some(map) = content.as_object() else {
                return Ok(false);
            };
            Ok(["problem", "procedure", "input", "output", "tests"]
                .iter()
                .all(|key| map.get(*key).is_some_and(|v| v.is_string() || v.is_array())))
        })),
        "instructions" => Some(Arc::new(|_: &str, content: &Value| {
            let map = content.as_object().ok_or("schema")?;
            Ok(map.contains_key("instructions"))
        })),
        _ => None,
    }
}

/// Ids as the generator's `randomUUID` replacement makes them.
fn counter() -> (Arc<AtomicU64>, Arc<dyn Fn() -> String + Send + Sync>) {
    let next = Arc::new(AtomicU64::new(0));
    let ids = next.clone();
    (
        next,
        Arc::new(move || format!("00000000-0000-4000-8000-{:012}", ids.fetch_add(1, Ordering::SeqCst) + 1)),
    )
}

/// The documents as `JSON.stringify([...values.entries()])`.
fn documents_text(documents: &Documents) -> String {
    let entries: Vec<Value> = documents
        .values
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| json!([k, v]))
        .collect();
    stringify(&Value::Array(entries))
}

#[test]
fn services_local_refinements_and_experiments_match_native_local_services() {
    let mut requests = 0;
    for session in fixture().as_array().unwrap() {
        let name = session["name"].as_str().unwrap();
        let documents = Documents::default();
        let (_, ids) = counter();
        let open = |options: &Value| {
            LocalServices::new(
                Box::new(documents.clone()),
                LocalOptions {
                    validate: validator(options["validate"].as_str().unwrap_or("none")),
                    require_approval: options["requireApproval"] == true,
                    now: Arc::new(|| 1_700_000_000_000),
                    ids: ids.clone(),
                },
            )
        };
        let mut services = open(&session["options"]);
        let mut branch: Option<Branch> = None;
        for (index, step) in session["steps"].as_array().unwrap().iter().enumerate() {
            let args = step["step"].as_array().unwrap();
            let kind = args[0].as_str().unwrap();
            match kind {
                "reopen" => services = open(&args[1]),
                "branch" => {
                    let path: Vec<String> = args[2]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|e| e.as_str().unwrap().to_string())
                        .collect();
                    branch = Some(Branch {
                        anchor: args[1].as_str().map(str::to_string),
                        on_branch: Some(Arc::new(move |anchor: &str| path.iter().any(|e| e == anchor))),
                    });
                }
                "unbranch" => branch = None,
                "seed" => {
                    documents
                        .values
                        .lock()
                        .unwrap()
                        .insert(args[1].as_str().unwrap().into(), args[2].clone());
                }
                "failWrites" => documents.fail.store(args[1] == true, Ordering::SeqCst),
                _ => {
                    requests += 1;
                    let outcome = services.handle(kind, &args[1], branch.as_ref());
                    let at = format!("{name} / step {index} {kind} {}", stringify(&args[1]));
                    match (&step.get("ok"), &step.get("error"), outcome) {
                        (Some(want), _, Ok(got)) => assert_eq!(stringify(&got), stringify(want), "{at}"),
                        (_, Some(want), Err(got)) => assert_eq!(got, want.as_str().unwrap(), "{at}"),
                        (want, error, got) => panic!("{at}: want {want:?} / {error:?}, got {got:?}"),
                    }
                    assert_eq!(
                        documents_text(&documents),
                        step["documents"].as_str().unwrap(),
                        "{at}: documents"
                    );
                }
            }
        }
    }
    assert_eq!(requests, 245);
}
