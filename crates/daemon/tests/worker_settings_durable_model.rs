//! `durable.model` in a durable-instance worker's settings (crate::worker_settings; user requirement, literal
//! expectations): only a worker of the durable instance (config::settings `set_durable_process`) reports it, with
//! the built-in default when unset; `/settings` saves it (a provider/model or null to clear it) for the next new
//! session, leaving the global default model alone. Its own test binary: the durable mark is per process.

use config::settings::{SettingsManager, set_durable_process};
use daemon::worker_settings::{self, Live};
use serde_json::{Value, json};

fn live() -> Live {
    Live {
        compaction: true,
        retry: true,
        steering_mode: "one-at-a-time".into(),
        follow_up_mode: "one-at-a-time".into(),
    }
}

#[test]
fn daemon_worker_settings_durable_model_is_a_durable_workers_setting() {
    let tmp = tempfile::tempdir().unwrap();
    let (agent, cwd) = (tmp.path().join("agent"), tmp.path().join("cwd"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        r#"{"defaultProvider":"cliproxyapi","defaultModel":"gpt-6-astra"}"#,
    )
    .unwrap();
    let env = |_: &str| None;
    let values = |settings: &SettingsManager| worker_settings::values(settings, &live(), &env).unwrap();
    let mut settings = SettingsManager::create(&cwd, &agent, false);
    // Not a durable worker: no such value.
    assert_eq!(values(&settings).get("durable.model"), None);
    set_durable_process();
    assert_eq!(values(&settings)["durable.model"], "claude-code/claude-opus-5-5");
    let (applied, changes) = worker_settings::apply(
        &mut settings,
        "durable.model",
        &json!("anthropic/claude-haiku-4-5"),
        None,
    )
    .unwrap();
    assert_eq!((applied.name(), changes.len()), ("restart", 0));
    let file =
        || -> Value { serde_json::from_str(&std::fs::read_to_string(agent.join("settings.json")).unwrap()).unwrap() };
    assert_eq!(
        file(),
        json!({"defaultProvider": "cliproxyapi", "defaultModel": "gpt-6-astra", "durable": {"model": "anthropic/claude-haiku-4-5"}})
    );
    assert_eq!(values(&settings)["durable.model"], "anthropic/claude-haiku-4-5");
    assert_eq!(
        worker_settings::apply(&mut settings, "durable.model", &json!("opus"), None).unwrap_err(),
        "durable.model must be provider/model (or null to clear it)"
    );
    worker_settings::apply(&mut settings, "durable.model", &Value::Null, None).unwrap();
    assert_eq!(
        file(),
        json!({"defaultProvider": "cliproxyapi", "defaultModel": "gpt-6-astra", "durable": {}})
    );
    assert_eq!(values(&settings)["durable.model"], "claude-code/claude-opus-5-5");
}
