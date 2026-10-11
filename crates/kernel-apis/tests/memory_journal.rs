//! Replays fixtures/memory-journal.json (generate-memory-journal.mjs): the TS NativeMemoryService reopened over a
//! journal it wrote and over corrupted variants. The Rust service must list the same operations (interrupted ones
//! settled as interrupted/unknown with OWNER_ENDED), refuse the same documents with the same error, and leave the
//! store holding the same document.

mod common;

use common::normalize;
use kernel::Signal;
use kernel_apis::memory::MemoryBackend;
use kernel_apis::util::stringify;
use kernel_apis::{DocumentStore, MemoryDocumentStore, MemoryService, worker_gate};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;

struct Bank;

impl MemoryBackend for Bank {
    fn namespace(&self) -> String {
        "fake://bank".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"]})
            .as_object()
            .unwrap()
            .clone()
    }
    fn recall(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        Err("unused".into())
    }
    fn retain(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        Err("unused".into())
    }
    fn get(&self, _: &str, _: &Signal) -> Result<Value, String> {
        Err("unused".into())
    }
    fn delete(&self, _: &str, _: &Signal) -> Result<Value, String> {
        Err("unused".into())
    }
    fn operation(&self, _: &str, _: &Signal) -> Result<Value, String> {
        Err("unused".into())
    }
}

#[test]
fn kernel_apis_memory_journal_reopen_matches_typescript() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/memory-journal.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let store = Arc::new(MemoryDocumentStore::new(Some(case["input"].clone())));
        let service = MemoryService::new(store.clone(), Arc::new(Bank), worker_gate()).unwrap();
        let mut produced = match service.list() {
            Ok(list) => json!({"ok": list}),
            Err(error) => json!({"error": error.to_string()}),
        };
        produced["stored"] = store.read().unwrap().unwrap_or(Value::Null);
        let produced: Value = serde_json::from_str(&normalize(&stringify(&produced), "\u{0}")).unwrap();
        let mut actual = json!({"name": case["name"], "input": case["input"]});
        for (key, value) in produced.as_object().unwrap() {
            actual[key] = value.clone();
        }
        assert_eq!(actual, *case, "{}", case["name"]);
    }
}
