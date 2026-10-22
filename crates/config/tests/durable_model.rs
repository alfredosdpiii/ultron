//! `durable.model` (config::settings `durable_model`, `set_durable_model`; user requirement, literal expectations):
//! the model the durable instance's new sessions start on, global only, `claude-code/claude-opus-5-5` (the Claude
//! subscription) when unset or not a `provider/model` reference; saving it merges into the `durable` object and
//! leaves the global default model alone.

use config::settings::{DURABLE_MODEL_DEFAULT, SettingsManager};
use serde_json::{Value, json};

#[test]
fn config_durable_model_is_global_only_with_the_subscription_default() {
    let dir = tempfile::tempdir().unwrap();
    let (agent, cwd) = (dir.path().join("agent"), dir.path().join("cwd"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    assert_eq!(DURABLE_MODEL_DEFAULT, "claude-code/claude-opus-5-5");
    let read = || SettingsManager::create(&cwd, &agent, true).durable_model();
    assert_eq!(read(), "claude-code/claude-opus-5-5");
    // A project cannot choose it (it would move the user's durable sessions off their subscription).
    std::fs::write(
        cwd.join(".pi/settings.json"),
        r#"{"durable":{"model":"openai/gpt-5.5"}}"#,
    )
    .unwrap();
    assert_eq!(read(), "claude-code/claude-opus-5-5");
    // Not a provider/model reference: the default.
    std::fs::write(agent.join("settings.json"), r#"{"durable":{"model":"opus"}}"#).unwrap();
    assert_eq!(read(), "claude-code/claude-opus-5-5");
    std::fs::write(
        agent.join("settings.json"),
        r#"{"defaultProvider":"cliproxyapi","defaultModel":"gpt-6-astra","durable":{"model":" anthropic/claude-haiku-4-5 ","keep":1}}"#,
    )
    .unwrap();
    assert_eq!(read(), "anthropic/claude-haiku-4-5");
    // Saving merges `model` into the object and touches nothing else.
    let mut settings = SettingsManager::create(&cwd, &agent, true);
    settings.set_durable_model("claude-code", "claude-haiku-5-5");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(agent.join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        file,
        json!({"defaultProvider": "cliproxyapi", "defaultModel": "gpt-6-astra", "durable": {"model": "claude-code/claude-haiku-5-5", "keep": 1}})
    );
    assert_eq!(read(), "claude-code/claude-haiku-5-5");
}
