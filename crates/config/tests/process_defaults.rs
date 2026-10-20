//! Process-wide settings defaults (config::settings `set_process_defaults`; the durable instance's session workers
//! get `persona: "ultron"`): under the global and project files, an explicit value in either wins, and nothing is
//! written to a file. Its own test binary: the defaults are set once per process.

use config::settings::{SettingsManager, process_defaults, set_process_defaults};
use serde_json::{Value, json};

#[test]
fn config_process_defaults_sit_under_the_files_and_are_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let (agent, cwd) = (dir.path().join("agent"), dir.path().join("cwd"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    assert!(process_defaults().is_none());
    let before = SettingsManager::create(&cwd, &agent, false);
    assert_eq!(before.settings().get("persona"), None);
    let Value::Object(defaults) = json!({"persona": "ultron", "nested": {"a": 1, "b": 2}}) else {
        unreachable!()
    };
    set_process_defaults(defaults.clone());
    // Set once: a second call changes nothing.
    let Value::Object(other) = json!({"persona": "other"}) else {
        unreachable!()
    };
    set_process_defaults(other);
    assert_eq!(process_defaults(), Some(&defaults));
    let none = SettingsManager::create(&cwd, &agent, false);
    assert_eq!(none.settings().get("persona"), Some(&json!("ultron")));
    assert!(none.global_settings().get("persona").is_none());
    // The user's explicit values win, objects merge.
    std::fs::write(agent.join("settings.json"), r#"{"persona": "off", "nested": {"b": 3}}"#).unwrap();
    let off = SettingsManager::create(&cwd, &agent, false);
    assert_eq!(off.settings().get("persona"), Some(&json!("off")));
    assert_eq!(off.settings().get("nested"), Some(&json!({"a": 1, "b": 3})));
    assert_eq!(
        std::fs::read_to_string(agent.join("settings.json")).unwrap(),
        r#"{"persona": "off", "nested": {"b": 3}}"#
    );
}
