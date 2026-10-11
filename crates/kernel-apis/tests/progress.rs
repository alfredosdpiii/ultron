//! Replays fixtures/progress.json, recorded by fixtures/generate-progress.mjs: `progress.*` cells through the real
//! runtime.py and crates/kernel against `ProgressModule` with the scripted task host of fake-tasks.mjs (receipts,
//! novelty by evidence key and hash, stall windows, owner/ancestor access, budget denials, verifier claims), and the
//! reload table (pending verifier claims settle as unverified; invalid stored documents are refused).

mod common;
mod fake_tasks;

use common::{FIXED_NOW, Made, assert_area, fixture};
use fake_tasks::FakeTasks;
use kernel_apis::{ApiModule, DocumentStore, KernelApis, MemoryDocumentStore, ProgressModule};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

fn progress_host(_: &Path) -> Made {
    let store = Arc::new(MemoryDocumentStore::default());
    let fake = FakeTasks::new(FIXED_NOW);
    let handler = KernelApis::new(fake.inner())
        .with_host(fake.clone(), "main")
        .with(Arc::new(ProgressModule::new(store.clone())));
    Made {
        handler,
        env: Vec::new(),
        state: Box::new(move || json!({"document": store.read().unwrap(), "log": fake.log()})),
    }
}

#[test]
fn kernel_apis_progress_cells_match_typescript_host() {
    assert_area("progress", &progress_host);
}

#[test]
fn kernel_apis_progress_reload_settles_pending_claims_and_refuses_invalid_documents() {
    for case in fixture("progress")["reload"].as_array().unwrap() {
        let store = Arc::new(MemoryDocumentStore::new(Some(case["input"].clone())));
        let module = ProgressModule::new(store.clone());
        let outcome = module.start(FakeTasks::new(FIXED_NOW).as_ref());
        match case.get("error") {
            Some(error) => assert_eq!(outcome, Err(error.as_str().unwrap().to_string()), "{case}"),
            None => {
                assert_eq!(outcome, Ok(()), "{case}");
                assert_eq!(store.read().unwrap().unwrap_or(Value::Null), case["document"], "{case}");
            }
        }
    }
}
