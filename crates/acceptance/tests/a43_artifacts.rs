//! A43 (the artifact cases): "a pinned checkpoint artifact survives delete and reclaim; unpinned artifacts are
//! reclaimed explicitly" and "quota failures are explicit and never evict artifacts to make room" of
//! packages/coding-agent/test/ultron-a43-resource-release.test.ts, over `kernel_apis::artifacts` and a real kernel
//! snapshot as the checkpoint.

mod common;

use common::{runtime, value};
use kernel::{Kernel, KernelOptions, Status};
use kernel_apis::artifacts::{ArtifactLimits, Artifacts, KeyedDocuments};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

/// Documents shared by a service and its reopened successor (`MemoryDocuments`).
#[derive(Clone, Default)]
struct Documents(Arc<Mutex<Map<String, Value>>>);

impl KeyedDocuments for Documents {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.into(), value.clone());
        Ok(())
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
}

fn service(documents: &Documents, limits: ArtifactLimits) -> Artifacts {
    Artifacts::new(Box::new(documents.clone()), limits).unwrap()
}

#[test]
fn acceptance_a43_a_pinned_checkpoint_artifact_survives_delete_and_reclaim_unpinned_artifacts_are_reclaimed_explicitly()
{
    let directory = tempfile::tempdir().unwrap();
    let mut options = KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime());
    options.snapshot_key = Some(b"acceptance a43 checkpoint key".to_vec());
    let kernel = Kernel::new(options, common::null_handler()).unwrap();
    value(&kernel, "state['step'] = 7");
    let snapshot = directory.path().join("checkpoint.snap");
    assert_eq!(kernel.snapshot(Some(&snapshot)).unwrap().result.status, Status::Ok);
    kernel.shutdown();
    let checkpoint_text = std::fs::read_to_string(&snapshot).unwrap();

    let documents = Documents::default();
    let services = service(&documents, ArtifactLimits::default());
    let put = |text: &str, options: Value| {
        services
            .handle("artifacts.put", &json!({ "text": text, "options": options }))
            .unwrap()
    };
    let checkpoint = put(
        &checkpoint_text,
        json!({ "label": "checkpoint", "mediaType": "application/json" }),
    );
    let scratch = put(&"x".repeat(500), json!({}));
    let evidence = put("test log: 12 passed", json!({}));
    assert_eq!(
        services
            .handle(
                "artifacts.pin",
                &json!({ "id": checkpoint["id"], "holder": "checkpoint:worker@1" })
            )
            .unwrap(),
        json!({ "id": checkpoint["id"], "holders": ["checkpoint:worker@1"] })
    );
    services
        .handle(
            "artifacts.pin",
            &json!({ "id": evidence["id"], "holder": "evidence:task-9" }),
        )
        .unwrap();
    let refused = services
        .handle("artifacts.delete", &json!({ "id": checkpoint["id"] }))
        .unwrap_err();
    assert!(refused.contains("retained by checkpoint:worker@1"), "{refused}");
    assert_eq!(
        services
            .handle("artifacts.reclaim", &json!({ "bytes": 1_000_000 }))
            .unwrap(),
        json!({ "deleted": [scratch["id"]], "freedBytes": 500, "satisfied": false,
                "retained": [checkpoint["id"], evidence["id"]] })
    );
    // The deleted artifact is explicitly invalidated, never a dangling or silently empty handle.
    assert_eq!(
        services
            .handle("artifacts.read", &json!({ "id": scratch["id"] }))
            .unwrap_err(),
        format!("Artifact {} was deleted (reclaimed)", scratch["id"].as_str().unwrap())
    );
    let mut listed: Vec<Value> = services
        .handle("artifacts.list", &json!({}))
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].clone())
        .collect();
    listed.sort_by_key(Value::to_string);
    let mut want = vec![checkpoint["id"].clone(), evidence["id"].clone()];
    want.sort_by_key(Value::to_string);
    assert_eq!(listed, want);

    // Retention survives a service restart; the checkpoint still restores exactly.
    let reopened = service(&documents, ArtifactLimits::default());
    assert!(
        reopened
            .handle("artifacts.delete", &json!({ "id": evidence["id"] }))
            .unwrap_err()
            .contains("retained by evidence:task-9")
    );
    let stored = reopened
        .handle(
            "artifacts.read",
            &json!({ "id": checkpoint["id"], "options": { "length": 1_048_576 } }),
        )
        .unwrap();
    assert_eq!(stored["text"], checkpoint_text);
    // Releasing the last holder allows deletion.
    reopened
        .handle(
            "artifacts.unpin",
            &json!({ "id": checkpoint["id"], "holder": "checkpoint:worker@1" }),
        )
        .unwrap();
    assert_eq!(
        reopened
            .handle(
                "artifacts.delete",
                &json!({ "id": checkpoint["id"], "reason": "checkpoint superseded" })
            )
            .unwrap(),
        json!({ "id": checkpoint["id"], "deleted": true })
    );
    assert!(
        reopened
            .handle("artifacts.read", &json!({ "id": checkpoint["id"] }))
            .unwrap_err()
            .contains("was deleted (checkpoint superseded)")
    );
    assert!(
        reopened
            .handle("artifacts.pin", &json!({ "id": checkpoint["id"], "holder": "late" }))
            .unwrap_err()
            .contains("was deleted")
    );
}

#[test]
fn acceptance_a43_quota_failures_are_explicit_and_never_evict_artifacts_to_make_room() {
    let services = service(
        &Documents::default(),
        ArtifactLimits {
            quota_bytes: Some(1_000),
            ..ArtifactLimits::default()
        },
    );
    let put = |text: String| services.handle("artifacts.put", &json!({ "text": text }));
    let first = put("a".repeat(600)).unwrap();
    services
        .handle("artifacts.pin", &json!({ "id": first["id"], "holder": "task:live" }))
        .unwrap();
    let second = put("b".repeat(300)).unwrap();
    assert_eq!(
        put("c".repeat(200)).unwrap_err(),
        "Artifact quota exceeded: 900 bytes used + 200 requested > 1000 byte quota. Delete or reclaim unpinned artifacts first."
    );
    // Nothing was removed to make room, and the rejected content was not stored.
    assert_eq!(
        services.handle("artifacts.usage", &json!({})).unwrap(),
        json!({ "usedBytes": 900, "quotaBytes": 1000, "count": 2, "pinned": [first["id"]] })
    );
    // Re-putting existing content deduplicates and needs no quota.
    assert_eq!(put("a".repeat(600)).unwrap()["id"], first["id"]);
    // Explicit reclaim frees only unpinned bytes; then the put succeeds.
    assert_eq!(
        services.handle("artifacts.reclaim", &json!({ "bytes": 200 })).unwrap(),
        json!({ "deleted": [second["id"]], "freedBytes": 300, "satisfied": true, "retained": [first["id"]] })
    );
    assert_eq!(put("c".repeat(200)).unwrap()["bytes"], 200);
    let usage = services.handle("artifacts.usage", &json!({})).unwrap();
    assert_eq!(
        (usage["usedBytes"].clone(), usage["count"].clone()),
        (json!(800), json!(2))
    );
    // A single artifact larger than the whole quota is rejected outright.
    assert!(put("d".repeat(1_001)).unwrap_err().contains("Artifact quota exceeded"));
}
