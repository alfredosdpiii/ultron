//! The model and thinking level a session starts on without `--model`/`--thinking` (`findInitialModel` and the
//! thinking rules of sdk.ts): the settings' default when its provider has credentials, else an available model
//! preferring a known provider's default; a resumed session keeps its own model. Private temp profile and an
//! injected environment: nothing reads the real environment or reaches the network.

use serde_json::{Value, json};
use session_core::core::lane_config;
use session_core::lane;
use session_core::{ModelFlags, RunEnv};
use session_journal::{SessionRepo, TestClock};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn env(dir: &Path, settings: Value, vars: &[(&str, &str)]) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(dir.join("cwd")).unwrap();
    let models = json!({"providers": {"stubproxy": {
        "baseUrl": "http://127.0.0.1:9/v1",
        "api": "openai-completions",
        "apiKey": "$STUB_KEY",
        "models": [{"id": "flash", "reasoning": true}, {"id": "plain", "reasoning": false}],
    }}});
    std::fs::write(agent.join("models.json"), models.to_string()).unwrap();
    std::fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
    let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    RunEnv {
        cwd: dir.join("cwd"),
        home: dir.to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

fn reference(model: &ai::Model) -> (String, String) {
    (model.provider.clone(), model.id.clone())
}

fn pair(provider: &str, id: &str) -> (String, String) {
    (provider.into(), id.into())
}

#[test]
fn session_core_initial_model_uses_the_settings_default_and_its_thinking_level() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({"defaultProvider": "stubproxy", "defaultModel": "flash", "defaultThinkingLevel": "high"});
    let env = env(dir.path(), settings, &[("STUB_KEY", "k")]);
    let model = lane::resolve_model(&ModelFlags::default(), &env).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "flash"));
    assert!(model.request.is_some(), "resolved for requests");
    let (config, model) = lane_config(&ModelFlags::default(), &env, None).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "flash"));
    assert_eq!(config["thinkingLevel"], "high");
    // --thinking still wins over the settings.
    let flags = ModelFlags {
        thinking: Some("low".into()),
        ..ModelFlags::default()
    };
    assert_eq!(lane_config(&flags, &env, None).unwrap().0["thinkingLevel"], "low");
}

#[test]
fn session_core_initial_thinking_prefers_the_per_model_level_over_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({
        "defaultProvider": "stubproxy", "defaultModel": "flash", "defaultThinkingLevel": "high",
        "modelThinkingLevels": {"stubproxy/flash": "minimal"},
    });
    let env = env(dir.path(), settings, &[("STUB_KEY", "k")]);
    assert_eq!(
        lane_config(&ModelFlags::default(), &env, None).unwrap().0["thinkingLevel"],
        "minimal"
    );
    // Another model takes the default level, stored unclamped as TS stores it (requests clamp it).
    let plain = ModelFlags {
        model: Some("stubproxy/plain".into()),
        ..ModelFlags::default()
    };
    assert_eq!(lane_config(&plain, &env, None).unwrap().0["thinkingLevel"], "high");
}

#[test]
fn session_core_initial_model_skips_a_default_without_credentials_and_prefers_a_known_providers_default() {
    let dir = tempfile::tempdir().unwrap();
    // The saved default's provider has no key: fall back to the available models, where anthropic's known default
    // wins over the first available model.
    let settings = json!({"defaultProvider": "stubproxy", "defaultModel": "flash"});
    let env = env(dir.path(), settings, &[("ANTHROPIC_API_KEY", "k")]);
    let model = lane::resolve_model(&ModelFlags::default(), &env).unwrap();
    assert_eq!(reference(&model), pair("anthropic", "claude-opus-4-8"));
}

#[test]
fn session_core_initial_model_falls_back_to_the_first_available_model() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), json!({}), &[("STUB_KEY", "k")]);
    let model = lane::resolve_model(&ModelFlags::default(), &env).unwrap();
    assert_eq!(model.provider, "stubproxy");
}

#[test]
fn session_core_initial_model_without_any_credentials_asks_for_a_model() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({"defaultProvider": "stubproxy", "defaultModel": "flash"});
    let env = env(dir.path(), settings, &[]);
    assert_eq!(
        lane::resolve_model(&ModelFlags::default(), &env).unwrap_err(),
        "No model selected: pass --model <provider>/<id>"
    );
}

#[test]
fn session_core_initial_model_never_replaces_a_resumed_sessions_model() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({"defaultProvider": "stubproxy", "defaultModel": "flash", "defaultThinkingLevel": "high"});
    let env = env(dir.path(), settings, &[("STUB_KEY", "k")]);
    let repo = SessionRepo::new(&dir.path().join("sessions"), TestClock::shared(1_790_000_000_000, 1));
    let mut session = repo.create("/w", None, None).unwrap();
    let plain = ModelFlags {
        model: Some("stubproxy/plain".into()),
        thinking: Some("low".into()),
        ..ModelFlags::default()
    };
    lane_config(&plain, &env, Some(&mut session)).unwrap();
    // Resumed without --model: the session's own model and thinking level, not the settings' default.
    let (config, model) = lane_config(&ModelFlags::default(), &env, Some(&mut session)).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "plain"));
    assert_eq!(config["thinkingLevel"], "low");
}

/// `resolveCliModel` behind `--model`: a partial id resolves within the provider, and a `:<thinking>` suffix names
/// the thinking level unless `--thinking` does (main.ts `cliThinkingFromModel`, the worker's `resolved.thinkingLevel`).
#[test]
fn session_core_model_flag_resolves_fuzzy_patterns_and_a_thinking_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(
        dir.path(),
        json!({"defaultThinkingLevel": "minimal"}),
        &[("STUB_KEY", "k")],
    );
    let fuzzy = ModelFlags {
        model: Some("stubproxy/fla:high".into()),
        ..ModelFlags::default()
    };
    let (config, model) = lane_config(&fuzzy, &env, None).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "flash"));
    assert_eq!(config["thinkingLevel"], "high");
    let explicit = ModelFlags {
        thinking: Some("low".into()),
        ..fuzzy.clone()
    };
    assert_eq!(lane_config(&explicit, &env, None).unwrap().0["thinkingLevel"], "low");
    // No suffix: the settings' level.
    let plain = ModelFlags {
        model: Some("stubproxy/pla".into()),
        ..ModelFlags::default()
    };
    let (config, model) = lane_config(&plain, &env, None).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "plain"));
    assert_eq!(config["thinkingLevel"], "minimal");
    // An invalid suffix is part of the id in strict CLI mode: a custom id on the provider.
    let odd = ModelFlags {
        model: Some("stubproxy/flash:turbo".into()),
        ..ModelFlags::default()
    };
    assert_eq!(lane::resolve_model(&odd, &env).unwrap().id, "flash:turbo");
}

/// Scoped models (`enabledModels`, or `--models` over it) pick a new session's model: the saved default when it is
/// in scope, else the first scoped model, with the pattern's thinking level (main.ts `buildSessionOptions`, then
/// sdk.ts's settings level when the pattern names none).
#[test]
fn session_core_scoped_models_pick_a_new_sessions_model() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({"enabledModels": ["stubproxy/plain:low", "stubproxy/fla*"], "defaultThinkingLevel": "high"});
    let env = env(dir.path(), settings, &[("STUB_KEY", "k")]);
    let (config, model) = lane_config(&ModelFlags::default(), &env, None).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "plain"));
    assert_eq!(config["thinkingLevel"], "low");
    // `--models` replaces `enabledModels`; a pattern without a level takes the settings' level.
    let flags = ModelFlags {
        models: Some(vec!["flash".into()]),
        ..ModelFlags::default()
    };
    let (config, model) = lane_config(&flags, &env, None).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "flash"));
    assert_eq!(config["thinkingLevel"], "high");
}

#[test]
fn session_core_scoped_models_prefer_the_saved_default_in_scope() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({
        "enabledModels": ["stubproxy/plain", "stubproxy/flash:xhigh"],
        "defaultProvider": "stubproxy", "defaultModel": "flash",
    });
    let env = env(dir.path(), settings, &[("STUB_KEY", "k")]);
    let (config, model) = lane_config(&ModelFlags::default(), &env, None).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "flash"));
    assert_eq!(config["thinkingLevel"], "xhigh");
}

/// Patterns match only available models: without credentials the scope is empty and the usual fallback applies.
#[test]
fn session_core_scoped_models_only_match_available_models() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({"enabledModels": ["stubproxy/*"]});
    let env = env(dir.path(), settings, &[("ANTHROPIC_API_KEY", "k")]);
    let model = lane::resolve_model(&ModelFlags::default(), &env).unwrap();
    assert_eq!(reference(&model), pair("anthropic", "claude-opus-4-8"));
}

/// A resumed session keeps its own model: scoped models do not move it (findInitialModel skips them when continuing).
#[test]
fn session_core_scoped_models_never_replace_a_resumed_sessions_model() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({"enabledModels": ["stubproxy/flash:max"]});
    let env = env(dir.path(), settings, &[("STUB_KEY", "k")]);
    let repo = SessionRepo::new(&dir.path().join("sessions"), TestClock::shared(1_790_000_000_000, 1));
    let mut session = repo.create("/w", None, None).unwrap();
    let plain = ModelFlags {
        model: Some("stubproxy/plain".into()),
        ..ModelFlags::default()
    };
    lane_config(&plain, &env, Some(&mut session)).unwrap();
    let (config, model) = lane_config(&ModelFlags::default(), &env, Some(&mut session)).unwrap();
    assert_eq!(reference(&model), pair("stubproxy", "plain"));
    assert_eq!(config["thinkingLevel"], "medium");
}
