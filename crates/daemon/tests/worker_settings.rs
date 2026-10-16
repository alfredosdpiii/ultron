//! Replays `fixtures/worker-settings.json` (recorded by `fixtures/generate-worker-settings.mjs` from the TypeScript
//! worker-settings.ts and session-control-provider.ts) against `daemon::worker_settings`, in temp profile directories.

use config::settings::SettingsManager;
use daemon::worker_settings::{self, Live};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/worker-settings.json")).unwrap()
}

/// A case's profile and project directories with its files (as the generator lays them out).
fn dirs(root: &Path, case: usize, files: &Value) -> (PathBuf, PathBuf) {
    let base = root.join(format!("case-{case}"));
    let (agent, cwd) = (base.join("agent"), base.join("project"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    if let Some(raw) = files["raw"].as_str() {
        std::fs::write(agent.join("settings.json"), raw).unwrap();
    } else if !files["global"].is_null() {
        std::fs::write(agent.join("settings.json"), files["global"].to_string()).unwrap();
    }
    if !files["project"].is_null() {
        std::fs::write(cwd.join(".pi/settings.json"), files["project"].to_string()).unwrap();
    }
    if let Some(trust) = files["trust"].as_array() {
        let updates: Vec<config::trust::TrustUpdate> = trust
            .iter()
            .map(|t| config::trust::TrustUpdate {
                path: match t["path"].as_str().unwrap() {
                    "." => cwd.clone(),
                    ".." => base.clone(),
                    other => PathBuf::from(other),
                },
                decision: t["decision"].as_bool(),
            })
            .collect();
        config::trust::ProjectTrustStore::new(&agent)
            .set_many(&updates)
            .unwrap();
    }
    (agent, cwd)
}

fn normalized(value: Value, root: &Path) -> Value {
    let text = value.to_string().replace(&root.display().to_string(), "<root>");
    serde_json::from_str(&text).unwrap()
}

fn env_of(case: &Value) -> HashMap<String, String> {
    case.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn daemon_worker_settings_read_matches_typescript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    for (index, case) in fixture()["reads"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let (agent, cwd) = dirs(&root, index + 1, &case["files"]);
        let settings = SettingsManager::create(&cwd, &agent, case["projectTrusted"] == true);
        let live = Live {
            compaction: case["live"]["compaction"] == true,
            retry: case["live"]["retry"] == true,
            steering_mode: case["live"]["steeringMode"].as_str().unwrap().into(),
            follow_up_mode: case["live"]["followUpMode"].as_str().unwrap().into(),
        };
        let env = env_of(&case["env"]);
        let read = worker_settings::read(&settings, &live, &cwd, &agent, &|name| env.get(name).cloned()).unwrap();
        assert_eq!(normalized(read, &root), case["read"], "{name}");
    }
}

#[test]
fn daemon_worker_settings_apply_matches_typescript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    for (index, case) in fixture()["applies"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let (agent, cwd) = dirs(&root, index + 1, &case["files"]);
        let mut settings = SettingsManager::create(&cwd, &agent, false);
        let model = (
            case["model"]["provider"].as_str().unwrap(),
            case["model"]["id"].as_str().unwrap(),
        );
        for step in case["steps"].as_array().unwrap() {
            let key = step["key"].as_str().unwrap();
            let outcome = worker_settings::apply(&mut settings, key, &step["value"], Some(model));
            let label = format!("{name}: {key} = {}", step["value"]);
            match outcome {
                Ok((applied, changes)) => {
                    assert_eq!(json!({"applied": applied.name()}), step["result"], "{label}");
                    let live: Vec<String> = changes.iter().map(|c| c.describe()).collect();
                    assert_eq!(json!(live), step["live"], "{label}");
                }
                Err(error) => {
                    assert_eq!(Some(error.as_str()), step["error"].as_str(), "{label}");
                    assert_eq!(step["live"], json!([]), "{label}");
                }
            }
            let text = std::fs::read_to_string(agent.join("settings.json")).unwrap();
            let file = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
            assert_eq!(normalized(file, &root), step["file"], "{label}: settings.json");
        }
    }
}

#[test]
fn daemon_worker_settings_model_env_overrides_match_typescript() {
    for case in fixture()["envOverrides"].as_array().unwrap() {
        let env = env_of(&case["env"]);
        let overrides = worker_settings::model_env_overrides(&|name| env.get(name).cloned());
        assert_eq!(Value::Object(overrides), case["overrides"], "{}", case["env"]);
    }
}

#[test]
fn daemon_worker_settings_debug_environment_redacts_like_typescript() {
    for case in fixture()["debugEnvironments"].as_array().unwrap() {
        let env = env_of(&case["env"]);
        let shown = worker_settings::debug_environment(env);
        // Key order is part of the contract (`/debug` prints the object as is).
        let keys: Vec<&String> = shown.keys().collect();
        let expected: Vec<&String> = case["environment"].as_object().unwrap().keys().collect();
        assert_eq!(keys, expected);
        assert_eq!(Value::Object(shown), case["environment"]);
    }
}

#[test]
fn daemon_worker_settings_debug_environment_sorts_like_locale_compare() {
    // `["PI_AB", "PI_A_B", "PI_A1", "PI_Ab", "PI_ab"].sort((a, b) => a.localeCompare(b))` in Node.
    let vars = ["PI_AB", "PI_A_B", "PI_A1", "PI_Ab", "PI_ab"].map(|n| (n.to_string(), "v".to_string()));
    let shown = worker_settings::debug_environment(vars);
    let keys: Vec<&str> = shown.keys().map(String::as_str).collect();
    assert_eq!(keys, ["PI_A_B", "PI_A1", "PI_ab", "PI_Ab", "PI_AB"]);
}
