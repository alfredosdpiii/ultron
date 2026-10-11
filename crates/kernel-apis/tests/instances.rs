//! Replays fixtures/instances.json, recorded by fixtures/generate-instances.mjs: `instances.*` cells through the real
//! runtime.py and crates/kernel against `InstancesModule` with the scripted task host of fake-tasks.mjs (retain rules,
//! lane names, best-effort pinning with two pins, busy and idempotent invocations, close/unpin, owner scoping), then a
//! restart that re-pins the open instances.

mod common;
mod fake_tasks;

use common::{FIXED_NOW, Made, assert_area};
use fake_tasks::FakeTasks;
use kernel_apis::{ApiModule, DocumentStore, InstancesModule, KernelApis, MemoryDocumentStore};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

fn instances_host(_: &Path) -> Made {
    let store = Arc::new(MemoryDocumentStore::default());
    let fake = FakeTasks::new(FIXED_NOW);
    let handler = KernelApis::new(fake.inner())
        .with_host(fake.clone(), "main")
        .with(Arc::new(InstancesModule::new(store.clone(), None)));
    Made {
        handler,
        env: Vec::new(),
        state: Box::new(move || {
            let document = store.read().unwrap();
            let restarted = FakeTasks::new(FIXED_NOW);
            InstancesModule::new(Arc::new(MemoryDocumentStore::new(document.clone())), None)
                .start(restarted.as_ref())
                .unwrap();
            json!({"document": document, "log": fake.log(), "restart": restarted.log()})
        }),
    }
}

#[test]
fn kernel_apis_instances_cells_match_typescript_host() {
    assert_area("instances", &instances_host);
}
