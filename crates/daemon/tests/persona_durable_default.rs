//! A durable-instance worker has `persona: "ultron"` as a process settings default (crate::server
//! `is_durable_directory`, config::settings `set_process_defaults`): turning the persona off there is kept as an
//! explicit `"off"` (removing the key would bring the default back). Its own test binary: the defaults are per
//! process.

use config::settings::{SettingsManager, set_process_defaults};
use daemon::worker_settings;
use serde_json::{Value, json};

#[test]
fn daemon_worker_settings_persona_off_stays_off_under_the_durable_default() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent, cwd) = (tmp.path().join("agent"), tmp.path().join("cwd"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let Value::Object(defaults) = json!({"persona": "ultron"}) else {
        unreachable!()
    };
    set_process_defaults(defaults);
    let mut settings = SettingsManager::create(&cwd, &agent, false);
    assert!(resources::persona::enabled(settings.settings()));
    worker_settings::apply(&mut settings, "persona", &json!("off"), None).unwrap();
    let file: Value = serde_json::from_str(&std::fs::read_to_string(agent.join("settings.json")).unwrap()).unwrap();
    assert_eq!(file, json!({"persona": "off"}));
    let reread = SettingsManager::create(&cwd, &agent, false);
    assert!(!resources::persona::enabled(reread.settings()));
    // On again: saved as `ultron`.
    worker_settings::apply(&mut settings, "persona", &json!("ultron"), None).unwrap();
    assert!(resources::persona::enabled(
        SettingsManager::create(&cwd, &agent, false).settings()
    ));
}
