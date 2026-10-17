//! Replays crates/config/fixtures/worker-settings.json (settings-manager.ts getters, session-worker.ts
//! `providerRequestOptions` and worker-settings.ts `workerProjectTrusted`, recorded by
//! fixtures/generate-worker-settings.mjs) against `config::worker_settings_keys` and `config::trust`.

use std::fs;
use std::path::Path;

use config::settings::{Settings, SettingsManager, migrate};
use config::worker_settings_keys::{self as keys, ProviderRetrySettings, RlmModelSettings, WorktreeSettings};
use serde_json::{Map, Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/worker-settings.json")).expect("fixture parses")
}

fn ok(value: Value) -> Value {
    json!({ "value": value })
}

fn result(value: Result<Value, String>) -> Value {
    match value {
        Ok(v) => ok(v),
        Err(e) => json!({ "error": e }),
    }
}

fn rlm_json(s: RlmModelSettings) -> Value {
    let mut out = Map::new();
    for (key, value) in [
        ("frameModel", s.frame_model),
        ("childModel", s.child_model),
        ("frameThinking", s.frame_thinking),
    ] {
        if let Some(v) = value {
            out.insert(key.into(), v.into());
        }
    }
    Value::Object(out)
}

fn worktree_json(s: WorktreeSettings) -> Value {
    let mut out = Map::new();
    if let Some(link) = s.link {
        out.insert("link".into(), json!(link));
    }
    if let Some(copy) = s.copy {
        out.insert("copy".into(), json!(copy));
    }
    if let Some(command) = s.command {
        out.insert("command".into(), command.into());
    }
    Value::Object(out)
}

fn retry_json(s: ProviderRetrySettings) -> Value {
    let mut out = Map::new();
    if let Some(t) = s.timeout_ms {
        out.insert("timeoutMs".into(), t);
    }
    if let Some(r) = s.max_retries {
        out.insert("maxRetries".into(), r);
    }
    out.insert("maxRetryDelayMs".into(), s.max_retry_delay_ms);
    Value::Object(out)
}

fn manager(dir: &Path, settings: &Value) -> SettingsManager {
    let agent = dir.join("agent");
    fs::create_dir_all(&agent).unwrap();
    fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
    SettingsManager::create(dir, &agent, false)
}

#[test]
fn config_worker_settings_getters_match_ts() {
    for case in fixture()["getters"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let label = case["settings"].to_string();
        let m = manager(dir.path(), &case["settings"]);
        let merged: Settings = migrate(case["settings"].as_object().unwrap().clone());
        assert_eq!(
            ok(rlm_json(m.rlm_model_settings())),
            case["rlmModelSettings"],
            "{label}"
        );
        assert_eq!(
            ok(m.review_model().map_or(Value::Null, Value::from)),
            case["reviewModel"],
            "{label}"
        );
        assert_eq!(
            ok(worktree_json(m.worktree_settings())),
            case["worktreeSettings"],
            "{label}"
        );
        assert_eq!(
            ok(retry_json(m.provider_retry_settings())),
            case["providerRetrySettings"],
            "{label}"
        );
        assert_eq!(
            result(m.http_idle_timeout_ms().map(Value::from)),
            case["httpIdleTimeoutMs"],
            "{label}"
        );
        assert_eq!(
            result(keys::provider_request_options(&merged)),
            case["providerRequestOptions"],
            "{label}"
        );
        assert_eq!(
            result(m.provider_request_options()),
            case["providerRequestOptions"],
            "{label}"
        );
        assert_eq!(ok(m.transport()), case["transport"], "{label}");
        for terminal in case["terminal"].as_array().unwrap() {
            let env = terminal["env"].as_object().unwrap().clone();
            let var = move |key: &str| env.get(key).and_then(Value::as_str).map(str::to_string);
            assert_eq!(
                ok(m.show_hardware_cursor(&var).into()),
                terminal["showHardwareCursor"],
                "{label} {terminal}"
            );
            assert_eq!(
                ok(m.clear_on_shrink(&var).into()),
                terminal["clearOnShrink"],
                "{label} {terminal}"
            );
        }
    }
}

/// Lays out one trust scenario: `<dir>/agent` (settings, trust.json) and `<dir>/work/proj` (with `.pi/settings.json`).
pub fn layout(dir: &Path, fixture: &Value, scenario: &Value) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = fs::canonicalize(dir).unwrap();
    let agent = dir.join("agent");
    let cwd = dir.join("work").join("proj");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let mut global = fixture["global"].clone();
    if let Some(default) = scenario["defaultProjectTrust"].as_str() {
        global["defaultProjectTrust"] = default.into();
    }
    fs::write(agent.join("settings.json"), global.to_string()).unwrap();
    if scenario["projectFiles"] == true {
        fs::create_dir_all(cwd.join(".pi")).unwrap();
        fs::write(cwd.join(".pi/settings.json"), fixture["project"].to_string()).unwrap();
    }
    let trust = match &scenario["trust"] {
        Value::Null => None,
        Value::String(s) if s == "parent-false" => Some(json!({ dir.join("work").display().to_string(): false })),
        decision => Some(json!({ cwd.display().to_string(): decision })),
    };
    if let Some(trust) = trust {
        fs::write(agent.join("trust.json"), trust.to_string()).unwrap();
    }
    (cwd, agent)
}

#[test]
fn config_worker_project_trust_decides_which_settings_apply_like_ts() {
    let fixture = fixture();
    for scenario in fixture["trust"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("fakehome");
        fs::create_dir_all(&home).unwrap();
        let (cwd, agent) = layout(dir.path(), &fixture, scenario);
        let name = scenario["name"].as_str().unwrap();
        let trusted = config::trust::worker_project_trusted(&cwd, &agent, &home);
        assert_eq!(Value::Bool(trusted), scenario["trusted"], "{name}");
        let m = SettingsManager::for_worker(&cwd, &agent, &home);
        assert_eq!(m.is_project_trusted(), trusted, "{name}");
        let s = m.settings();
        let compaction = json!({
            "enabled": m.compaction_enabled(),
            "reserveTokens": s["compaction"].get("reserveTokens").cloned().unwrap_or(json!(16384)),
            "keepRecentTokens": s["compaction"].get("keepRecentTokens").cloned().unwrap_or(json!(20000)),
        });
        assert_eq!(ok(compaction), scenario["compaction"], "{name}");
        assert_eq!(ok(m.retry_settings()), scenario["retry"], "{name}");
        assert_eq!(
            ok(m.default_provider().unwrap_or(Value::Null)),
            scenario["defaultProvider"],
            "{name}"
        );
        assert_eq!(
            ok(m.default_model().unwrap_or(Value::Null)),
            scenario["defaultModel"],
            "{name}"
        );
        assert_eq!(
            ok(s.get("defaultThinkingLevel").cloned().unwrap_or(Value::Null)),
            scenario["defaultThinkingLevel"],
            "{name}"
        );
        assert_eq!(
            result(m.http_idle_timeout_ms().map(Value::from)),
            scenario["httpIdleTimeoutMs"],
            "{name}"
        );
        assert_eq!(
            ok(m.thinking_budgets().cloned().unwrap_or(Value::Null)),
            scenario["thinkingBudgets"],
            "{name}"
        );
        assert_eq!(ok(m.transport()), scenario["transport"], "{name}");
        assert_eq!(
            result(m.provider_request_options()),
            scenario["providerRequestOptions"],
            "{name}"
        );
        assert_eq!(
            ok(rlm_json(m.rlm_model_settings())),
            scenario["rlmModelSettings"],
            "{name}"
        );
        assert_eq!(
            ok(m.review_model().map_or(Value::Null, Value::from)),
            scenario["reviewModel"],
            "{name}"
        );
        assert_eq!(
            ok(worktree_json(m.worktree_settings())),
            scenario["worktreeSettings"],
            "{name}"
        );
    }
}
