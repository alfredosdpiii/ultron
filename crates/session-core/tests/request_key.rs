//! The key a request carries, replayed from `fixtures/request-key.json` (TypeScript `ModelRuntime.getAuth` with
//! `setRuntimeApiKey` for `--api-key`, recorded by `fixtures/generate-request-key.mjs`): `--api-key`, then the
//! auth.json credential (api key with its `$VAR` template, or an OAuth access token; a stored credential owns its
//! provider), then the models.json `apiKey`, then the environment. Private temp profiles with fake keys and an
//! injected environment: nothing reads the real environment, the real profile or the network.

use serde_json::Value;
use session_core::lane;
use session_core::{ModelFlags, RunEnv};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn env(dir: &Path, vars: &serde_json::Map<String, Value>) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let mut map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    RunEnv {
        cwd: dir.to_path_buf(),
        home: dir.to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

#[test]
fn session_core_request_key_matches_typescript_resolution_order() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("../fixtures/request-key.json")).expect("fixture parses");
    assert_eq!(cases.len(), 24);
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path(), case["env"].as_object().unwrap());
        let agent = env.agent_dir();
        if case["models"].is_object() {
            let models = serde_json::json!({"providers": case["models"]});
            std::fs::write(agent.join("models.json"), models.to_string()).unwrap();
        }
        match &case["auth"] {
            Value::Null => {}
            Value::String(raw) => std::fs::write(agent.join("auth.json"), raw).unwrap(),
            auth => std::fs::write(agent.join("auth.json"), auth.to_string()).unwrap(),
        }
        let flags = ModelFlags {
            api_key: case["apiKeyFlag"].as_str().map(str::to_string),
            ..ModelFlags::default()
        };
        let provider = case["provider"].as_str().unwrap();
        let got = lane::request_key(&flags, &env, provider);
        match case.get("error").and_then(Value::as_str) {
            // JSON parse messages are the parser's own; the request fails with the store's prefix either way.
            Some(error) if error.starts_with("Credential store read failed") => {
                let prefix = format!("Credential store read failed for {provider}: ");
                assert!(error.starts_with(&prefix), "{name}: fixture {error}");
                let got = got.expect_err(name);
                assert!(got.starts_with(&prefix), "{name}: {got}");
            }
            Some(error) => assert_eq!(got, Err(error.to_string()), "{name}"),
            None => assert_eq!(got, Ok(case["apiKey"].as_str().map(str::to_string)), "{name}"),
        }
    }
}

/// `api_key` (what every lane request uses) is `request_key` without its error: the stored key reaches lanes.
#[test]
fn session_core_request_key_stored_key_reaches_lane_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), &serde_json::Map::new());
    std::fs::write(
        env.agent_dir().join("auth.json"),
        r#"{"anthropic": {"type": "api_key", "key": "sk-stored-fake"}}"#,
    )
    .unwrap();
    assert_eq!(
        lane::api_key(&ModelFlags::default(), &env, "anthropic").as_deref(),
        Some("sk-stored-fake")
    );
    assert_eq!(lane::api_key(&ModelFlags::default(), &env, "openai"), None);
}
