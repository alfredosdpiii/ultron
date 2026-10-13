//! Replays fixtures/schema.json (recorded by fixtures/generate-schema.mjs from definition-registry.ts and typebox
//! 1.3): the meta-check of every schema, typebox's `Check` and `Errors` (keyword, path, params, message, order) over
//! the corpus, and the registry's register / list / stored hashes / reload / validation errors.

use delegation::definitions::DefinitionRegistry;
use delegation::schema::{check, errors, validate_json_schema};
use delegation::task_store::TaskStore;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/schema.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Option<Value>>>);

struct Store {
    shared: Shared,
    fail_read: bool,
    fail_write: bool,
}

impl TaskStore for Store {
    fn read(&mut self) -> Result<Option<Value>, String> {
        if self.fail_read {
            return Err("disk".into());
        }
        Ok(self.shared.0.lock().unwrap().clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        if self.fail_write {
            return Err("full".into());
        }
        *self.shared.0.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

fn store(shared: &Shared) -> Box<dyn TaskStore> {
    Box::new(Store {
        shared: shared.clone(),
        fail_read: false,
        fail_write: false,
    })
}

fn outcome(result: Result<Value, String>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": error }),
    }
}

#[test]
fn delegation_schema_meta_check_matches_validate_json_schema() {
    let fixture = fixture();
    for case in fixture["validate"].as_array().unwrap() {
        let got = outcome(validate_json_schema(&case["schema"], "$").map(|()| Value::Null));
        let mut want = case.clone();
        want.as_object_mut().unwrap().remove("schema");
        assert_eq!(got, want, "{}", case["schema"]);
    }
}

#[test]
fn delegation_schema_check_and_errors_match_typebox() {
    let fixture = fixture();
    for case in fixture["check"].as_array().unwrap() {
        let (schema, value) = (&case["schema"], &case["value"]);
        assert_eq!(check(schema, value), case["check"], "check {schema} {value}");
        let got: Vec<Value> = errors(schema, value).iter().map(|e| e.to_json()).collect();
        assert_eq!(Value::from(got), case["errors"], "errors {schema} {value}");
    }
}

#[test]
fn delegation_registry_matches_definition_registry() {
    let fixture = fixture();
    let shared = Shared::default();
    let mut registry = DefinitionRegistry::new(Some(store(&shared)), true, true);
    let mut stored_at_list = Value::Null;
    for op in fixture["registry"].as_array().unwrap() {
        let name = op["op"].as_str().unwrap();
        let args = &op["args"];
        let got = match name {
            "register" => outcome(registry.register(args)),
            "list" => outcome(Ok(Value::from(registry.list()))),
            "stored" => {
                stored_at_list = shared.0.lock().unwrap().clone().unwrap();
                outcome(Ok(stored_at_list.clone()))
            }
            "get" => outcome(
                registry
                    .get(args.as_str().unwrap())
                    .map(|d| json!({ "hash": d.hash, "strategy": d.strategy().as_str() })),
            ),
            "validation" => {
                let definition = registry.get(args[0].as_str().unwrap()).unwrap();
                let (value, direction) = (&args[1], args[2].as_str().unwrap());
                let valid = if direction == "input" {
                    registry.is_valid_input(&definition, value)
                } else {
                    registry.is_valid_output(&definition, value)
                };
                outcome(Ok(
                    json!({ "valid": valid, "error": registry.validation_error(&definition, value, direction) }),
                ))
            }
            "reload" => {
                let mut reopened = DefinitionRegistry::new(Some(store(&shared)), true, true);
                outcome(reopened.ready().map(|()| {
                    Value::from(
                        reopened
                            .list()
                            .iter()
                            .map(|d| format!("{}@{}", d["id"].as_str().unwrap(), d["version"].as_str().unwrap()))
                            .collect::<Vec<_>>(),
                    )
                }))
            }
            "tampered" | "conflict" | "version" | "unreadable" => {
                let mut document = stored_at_list.clone();
                match name {
                    "tampered" => {
                        let list = document["definitions"].as_array_mut().unwrap();
                        list.last_mut().unwrap()["hash"] = json!("0".repeat(64));
                    }
                    "conflict" => {
                        let list = document["definitions"].as_array_mut().unwrap();
                        let identity = list.iter_mut().find(|d| d["id"] == "identity").unwrap();
                        identity["instructions"] = json!("Something else.");
                        let mut descriptor = identity.clone();
                        descriptor.as_object_mut().unwrap().shift_remove("hash");
                        identity["hash"] = json!(delegation::definitions::definition_hash(&descriptor));
                    }
                    "version" => document = json!({ "version": 2, "definitions": [] }),
                    _ => {}
                }
                let other = Shared(Arc::new(Mutex::new(Some(document))));
                let mut registry = DefinitionRegistry::new(
                    Some(Box::new(Store {
                        shared: other,
                        fail_read: name == "unreadable",
                        fail_write: false,
                    })),
                    false,
                    false,
                );
                outcome(registry.ready().map(|()| Value::Null))
            }
            "no predict adapter" | "no deterministic adapter" => {
                let mut bare = DefinitionRegistry::new(Some(store(&Shared::default())), false, false);
                let (id, strategy) = if name == "no predict adapter" {
                    ("p", "predict")
                } else {
                    ("d", "deterministic")
                };
                let mut candidate = fixture["registry"][0]["args"].clone();
                candidate["id"] = json!(id);
                candidate["strategy"] = json!(strategy);
                candidate["instructions"] = json!(format!("Run {id}."));
                outcome(bare.register(&candidate))
            }
            "no store" | "no store identical builtin" => {
                let mut bare = DefinitionRegistry::new(None, false, false);
                let candidate = if name == "no store" {
                    let mut candidate = fixture["registry"][0]["args"].clone();
                    candidate["id"] = json!("r");
                    candidate["instructions"] = json!("Run r.");
                    candidate
                } else {
                    registry.list().into_iter().find(|d| d["id"] == "identity").unwrap()
                };
                outcome(bare.register(&candidate))
            }
            "failed write" | "after failed write" => {
                // One failing registry for both steps (created at the first).
                thread_local! {
                    static FAILING: std::cell::RefCell<Option<DefinitionRegistry>> = const { std::cell::RefCell::new(None) };
                }
                let id = if name == "failed write" { "r" } else { "s" };
                let mut candidate = fixture["registry"][0]["args"].clone();
                candidate["id"] = json!(id);
                candidate["instructions"] = json!(format!("Run {id}."));
                FAILING.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    let failing = slot.get_or_insert_with(|| {
                        DefinitionRegistry::new(
                            Some(Box::new(Store {
                                shared: Shared::default(),
                                fail_read: false,
                                fail_write: true,
                            })),
                            false,
                            false,
                        )
                    });
                    outcome(failing.register(&candidate))
                })
            }
            other => panic!("unknown op {other}"),
        };
        let mut want = op.clone();
        let want = want.as_object_mut().unwrap();
        want.remove("op");
        want.remove("args");
        assert_eq!(got, Value::Object(want.clone()), "{name} {args}");
    }
}
