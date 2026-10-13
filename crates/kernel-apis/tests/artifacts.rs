//! Replays fixtures/artifacts.json (recorded by fixtures/generate-artifacts.mjs from local-services.ts): every
//! `artifacts.*` request must return the same result or error and leave the same documents (as JSON text, key order
//! included) after every step, across a reopen of the service over the same documents.

use kernel_apis::artifacts::{ArtifactLimits, Artifacts, KeyedDocuments};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/artifacts.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A JavaScript `Map` shared with the test.
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

fn service(documents: &Documents, options: &Value) -> Artifacts {
    let limits = ArtifactLimits {
        max_artifact_bytes: options["maxArtifactBytes"].as_u64().unwrap_or(256 * 1024 * 1024),
        max_storage_bytes: options["maxArtifactStorageBytes"]
            .as_u64()
            .unwrap_or(1024 * 1024 * 1024),
        quota_bytes: options["artifactQuotaBytes"].as_u64(),
    };
    Artifacts::with_clock(Box::new(documents.clone()), limits, Arc::new(|| 1_700_000_000_000)).unwrap()
}

#[test]
fn kernel_apis_artifacts_match_native_local_services() {
    for session in fixture().as_array().unwrap() {
        let name = session["name"].as_str().unwrap();
        let documents = Documents::default();
        let mut artifacts = service(&documents, &session["options"]);
        for step in session["steps"].as_array().unwrap() {
            if step["step"][0] == "reopen" {
                artifacts = service(&documents, &session["options"]);
                continue;
            }
            let request = &step["request"];
            let got = match artifacts.handle(request[0].as_str().unwrap(), &request[1]) {
                Ok(result) => json!({ "result": result }),
                Err(error) => json!({ "error": error }),
            };
            let mut want = step.as_object().unwrap().clone();
            let stored = want.remove("documents").unwrap();
            want.remove("step");
            want.remove("request");
            assert_eq!(
                got.to_string(),
                Value::Object(want).to_string(),
                "{name}: {}",
                step["step"]
            );
            let ours = Value::Object(documents.0.lock().unwrap().clone());
            assert_eq!(
                ours.to_string(),
                stored.to_string(),
                "{name}: documents after {}",
                step["step"]
            );
        }
    }
}

#[test]
fn kernel_apis_artifact_limits_must_be_positive() {
    let zero = ArtifactLimits {
        max_artifact_bytes: 0,
        ..ArtifactLimits::default()
    };
    let error = Artifacts::new(Box::new(Documents::default()), zero).err().unwrap();
    assert_eq!(error, "Artifact limits must be positive integers");
}

#[test]
fn kernel_apis_artifact_base64_is_standard_padded() {
    for (bytes, want) in [
        (&b""[..], ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"\xff\xfe\xfd\xfc", "//79/A=="),
    ] {
        assert_eq!(kernel_apis::artifacts::base64(bytes), want);
    }
}
