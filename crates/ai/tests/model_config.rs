//! Replays `fixtures/model-config.json` (recorded from the TypeScript `ModelConfig.load` and `InMemoryModelsStore` by
//! `fixtures/generate-model-config.mjs`; its `flattenModelCatalog` case has no Rust port: the catalog is generated flat).

use ai::model_config::{
    InMemoryModelsStore, ModelConfig, ModelsStore, ModelsStoreEntry, models_json_warning, strip_json_comments,
};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/model-config.json")).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ai-model-config-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn ai_model_config_load_matches_typescript() {
    let root = temp_dir("load");
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 52);
    for (n, case) in cases.iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let dir = root.join(format!("case-{n}"));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("models.json");
        let mut path = file.display().to_string();
        if case["tilde"] == true {
            fs::write(home.join("models.json"), case["content"].as_str().unwrap()).unwrap();
            path = "~/models.json".into();
        } else if case["directory"] == true {
            fs::create_dir(&file).unwrap();
        } else if let Some(bytes) = case["hex"].as_str() {
            fs::write(&file, hex(bytes)).unwrap();
        } else if let Some(content) = case["content"].as_str() {
            fs::write(&file, content).unwrap();
            if let Some(mode) = case["mode"].as_u64() {
                fs::set_permissions(&file, fs::Permissions::from_mode(mode as u32)).unwrap();
            }
        }
        let config = ModelConfig::load_with_home((case["noPath"] != true).then_some(path.as_str()), &home);
        if case["mode"].is_u64() {
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let error = config.error().map(|e| {
            e.replace(&dir.display().to_string(), "<dir>")
                .replace(&home.display().to_string(), "<home>")
        });
        match (error.as_deref(), case["error"].as_str()) {
            // V8's JSON.parse wording is not ported: only the frame around it must match.
            (Some(actual), Some(expected)) if expected.starts_with("Failed to parse models.json: ") => {
                let suffix = &expected[expected.rfind("\n\nFile: ").unwrap()..];
                assert!(actual.starts_with("Failed to parse models.json: "), "{name}: {actual}");
                assert!(actual.ends_with(suffix), "{name}: {actual}");
            }
            (actual, expected) => assert_eq!(actual, expected, "{name}: error"),
        }
        let ids: Vec<&str> = config.provider_ids();
        assert_eq!(json!(ids), case["providerIds"], "{name}: ids");
        let providers: Vec<Value> = ids.iter().map(|id| json!([id, config.provider(id).unwrap()])).collect();
        assert_eq!(Value::Array(providers), case["providers"], "{name}: providers");
    }
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn ai_model_config_models_store_clones_entries() {
    let recorded = &fixture()["store"];
    let store = InMemoryModelsStore::default();
    let model = json!({"id": "m", "name": "M", "api": "openai-completions", "provider": "p", "baseUrl": "u",
        "reasoning": false, "input": ["text"], "cost": {"input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 1.25},
        "contextWindow": 1, "maxTokens": 1});
    let mut entry: ModelsStoreEntry =
        serde_json::from_value(json!({"models": [model], "lastModified": 1700000000, "etag": "\"abc\""})).unwrap();
    store.write("p", &entry);
    entry.models[0].name = "mutated after write".into();
    let mut first = store.read("p").unwrap();
    first.models[0].name = "mutated after read".into();
    let second = store.read("p").unwrap();
    let as_json = |e: &ModelsStoreEntry| ai::json::normalize_numbers(serde_json::to_value(e).unwrap());
    assert_eq!(as_json(&second), recorded["read"]);
    assert_eq!(store.read("q"), None);
    assert!(recorded["missing"].is_null());
    store.delete("p");
    assert_eq!(store.read("p"), None);
    assert!(recorded["afterDelete"].is_null());
}

#[test]
fn ai_model_config_strip_json_comments_keeps_strings() {
    // stripJsonComments('{"a":"x // y", // c\n "b":[1,],}') in node.
    assert_eq!(
        strip_json_comments("{\"a\":\"x // y\", // c\n \"b\":[1,],}"),
        "{\"a\":\"x // y\", \n \"b\":[1]}"
    );
    // An unterminated string does not protect what follows it.
    assert_eq!(strip_json_comments("\"a\\\n// b"), "\"a\\\n");
}

#[test]
fn ai_model_config_warning_text() {
    assert_eq!(
        models_json_warning("Failed to parse models.json: x"),
        "Warning: errors loading models.json:\nFailed to parse models.json: x"
    );
}
