//! Replays fixtures/kernel.json (recorded by fixtures/generate-kernel.mjs): Python cells calling `refinements.*`,
//! `gates.*` and `rlm.host_request` for `refinements.*` / `experiments.*` through the real runtime.py, served by
//! the TypeScript host (NativeRlmHost with NativeLocalServices and the release-gate module). Here the same cells run
//! through crates/kernel with `KernelApis` over this crate's modules and must print, return and fail the same, and
//! leave the same documents. The kernel-apis replay harness is shared (same normalization rules).

#[path = "../../kernel-apis/tests/common/mod.rs"]
mod common;

use common::{FIXED_NOW, Made, fixed, inner, run_scenario};
use kernel_apis::artifacts::{KeyedDocuments, MemoryKeyedDocuments};
use kernel_apis::{DocumentStore, KernelApis, MemoryDocumentStore};
use serde_json::{Map, Value, json};
use services::local::{LocalOptions, LocalServices, LocalServicesModule, Validator};
use services::{ReleaseGateModule, validate_refinement_content};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

/// `MemoryKeyedDocuments` the test can read back.
#[derive(Clone, Default)]
struct Shared(Arc<MemoryKeyedDocuments>);

impl KeyedDocuments for Shared {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        self.0.get(key)
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        self.0.set(key, value)
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        self.0.list(prefix)
    }
}

fn local_host(validate: Option<Validator>) -> impl Fn(&Path) -> Made {
    move |_dir: &Path| {
        let documents = Shared::default();
        let services = LocalServices::new(
            Box::new(documents.clone()),
            LocalOptions {
                validate: validate.clone(),
                ..LocalOptions::default()
            },
        );
        let gates = Arc::new(MemoryDocumentStore::default());
        Made {
            handler: KernelApis::new(inner(Arc::new(AtomicI64::new(FIXED_NOW))))
                .with(Arc::new(LocalServicesModule {
                    services: Arc::new(services),
                }))
                .with(Arc::new(ReleaseGateModule::new(gates.clone(), Some(fixed(FIXED_NOW))))),
            env: Vec::new(),
            state: Box::new(move || {
                let local: Map<String, Value> = documents.list("").unwrap().into_iter().collect();
                json!({"local": local, "gates": gates.read().unwrap()})
            }),
        }
    }
}

#[test]
fn services_kernel_apis_match_the_typescript_host() {
    let fixture = common::fixture("kernel");
    let plain = local_host(None);
    let worker = local_host(Some(Arc::new(validate_refinement_content)));
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 4);
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let make: &dyn Fn(&Path) -> Made = if name == "worker validation of skill proposals" {
            &worker
        } else {
            &plain
        };
        let actual = run_scenario(scenario, make);
        for (index, expected) in scenario["cells"].as_array().unwrap().iter().enumerate() {
            assert_eq!(actual["cells"][index], *expected, "{name} / cell {index}");
        }
        assert_eq!(actual["state"], scenario["state"], "{name} / state");
        assert_eq!(actual, *scenario, "{name}");
    }
}
