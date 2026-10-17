//! `<agentDir>/keybindings.json` against packages/coding-agent/src/core/keybindings.ts, replaying
//! fixtures/keybindings-file.json (generate-keybindings-file.mjs): `KeybindingsManager.create(agentDir)` (user
//! bindings, the effective config, conflicts), `migrateKeybindingsConfig` and `reload()`.

use std::path::Path;

use serde_json::{Map, Value};
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings};
use tui::keybindings_file::{UserBindings, keybindings_path, load_user_bindings, migrate_keybindings_config};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/keybindings-file.json")).unwrap()
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tui-keybindings-file-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, content: &Value) {
    let path = keybindings_path(dir);
    match content.as_str() {
        Some(text) => std::fs::write(path, text).unwrap(),
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The fixture's `[id, "key" | ["keys"]]` pairs as the manager's user bindings.
fn expected_user(pairs: &Value) -> UserBindings {
    pairs
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            let keys = match &pair[1] {
                Value::String(key) => vec![key.clone()],
                keys => keys
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| k.as_str().unwrap().to_string())
                    .collect(),
            };
            (pair[0].as_str().unwrap().to_string(), Some(keys))
        })
        .collect()
}

fn resolved_json(manager: &KeybindingsManager) -> Value {
    Value::Array(
        manager
            .get_resolved_bindings()
            .into_iter()
            // `getResolvedBindings`: one key as a string, otherwise the array.
            .map(|(id, keys)| match keys.as_slice() {
                [one] => serde_json::json!([id, one]),
                _ => serde_json::json!([id, keys]),
            })
            .collect(),
    )
}

fn conflicts_json(manager: &KeybindingsManager) -> Value {
    Value::Array(
        manager
            .get_conflicts()
            .into_iter()
            .map(|c| serde_json::json!({"key": c.key, "keybindings": c.keybindings}))
            .collect(),
    )
}

fn manager(user: UserBindings) -> KeybindingsManager {
    KeybindingsManager::new(app_keybindings(Platform::new("linux", false)), user, Default::default())
}

#[test]
fn tui_keybindings_file_load_and_migrate_match_ts_create() {
    let fixture = fixture();
    let definitions = app_keybindings(Platform::new("linux", false));
    let order: Vec<&str> = definitions.iter().map(|d| d.id).collect();
    let dir = temp_dir("create");
    for case in fixture["cases"].as_array().unwrap() {
        write(&dir, &case["content"]);
        let user = load_user_bindings(&keybindings_path(&dir), &definitions);
        assert_eq!(user, expected_user(&case["user"]), "{}", case["content"]);
        let m = manager(user);
        assert_eq!(resolved_json(&m), case["resolved"], "{}", case["content"]);
        assert_eq!(conflicts_json(&m), case["conflicts"], "{}", case["content"]);
        if let Some(migrated) = case["migrated"].as_object() {
            let text = case["content"].as_str().unwrap();
            let raw: Map<String, Value> = serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap();
            let (config, renamed) = migrate_keybindings_config(&raw, &order);
            let pairs: Vec<Value> = config.into_iter().map(|(k, v)| serde_json::json!([k, v])).collect();
            assert_eq!(Value::Array(pairs), migrated["config"], "{text}");
            assert_eq!(Value::Bool(renamed), migrated["migrated"], "{text}");
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tui_keybindings_file_reload_rereads_the_file_like_ts() {
    let fixture = fixture();
    let definitions = app_keybindings(Platform::new("linux", false));
    let dir = temp_dir("reload");
    for case in fixture["reloads"].as_array().unwrap() {
        write(&dir, &case["before"]);
        let mut m = manager(load_user_bindings(&keybindings_path(&dir), &definitions));
        write(&dir, &case["after"]);
        let user = load_user_bindings(&keybindings_path(&dir), &definitions);
        assert_eq!(user, expected_user(&case["user"]), "{}", case["after"]);
        m.set_user_bindings(user);
        assert_eq!(resolved_json(&m), case["resolved"], "{}", case["after"]);
    }
    let _ = std::fs::remove_dir_all(dir);
}
