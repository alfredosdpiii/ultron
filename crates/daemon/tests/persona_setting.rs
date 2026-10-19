//! The worker's `setSetting` takes `persona` (resources::persona): `ultron` is saved and applies live (the session
//! renders its prompt again on the next request), `off` or null removes it, anything else is refused.

use config::settings::SettingsManager;
use daemon::worker_settings::{self, Applied};
use serde_json::{Value, json};

fn file(agent: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(agent.join("settings.json")).unwrap()).unwrap()
}

#[test]
fn daemon_worker_settings_persona_is_saved_live_and_removed_by_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent, cwd) = (tmp.path().join("agent"), tmp.path().join("cwd"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(agent.join("settings.json"), json!({"theme": "dark"}).to_string()).unwrap();
    let mut settings = SettingsManager::create(&cwd, &agent, false);
    let (applied, changes) = worker_settings::apply(&mut settings, "persona", &json!("ultron"), None).unwrap();
    assert_eq!((applied, changes.len()), (Applied::Live, 0));
    assert_eq!(file(&agent), json!({"theme": "dark", "persona": "ultron"}));
    worker_settings::apply(&mut settings, "persona", &json!("off"), None).unwrap();
    assert_eq!(file(&agent), json!({"theme": "dark"}));
    worker_settings::apply(&mut settings, "persona", &json!("ultron"), None).unwrap();
    worker_settings::apply(&mut settings, "persona", &Value::Null, None).unwrap();
    assert_eq!(file(&agent), json!({"theme": "dark"}));
    let error = worker_settings::apply(&mut settings, "persona", &json!("jarvis"), None).unwrap_err();
    assert_eq!(error, "persona must be one of: ultron, off");
    assert_eq!(file(&agent), json!({"theme": "dark"}));
}
