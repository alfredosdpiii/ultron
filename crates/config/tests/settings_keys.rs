//! Replays crates/config/fixtures/settings-keys.json (settings-manager.ts getters recorded by
//! fixtures/generate-settings-keys.mjs) against `config::settings_keys`.

use std::fs;
use std::path::Path;

use config::settings::SettingsManager;
use config::settings_keys::{self as keys, CapabilityOverrides};
use serde_json::{Map, Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/settings-keys.json")).expect("fixture parses")
}

fn ok(value: Value) -> Value {
    json!({ "value": value })
}

fn overrides_json(o: CapabilityOverrides) -> Value {
    let mut out = Map::new();
    if let Some(images) = o.images {
        out.insert("images".into(), images.map_or(Value::Null, Value::from));
    }
    if let Some(b) = o.true_color {
        out.insert("trueColor".into(), b.into());
    }
    if let Some(b) = o.hyperlinks {
        out.insert("hyperlinks".into(), b.into());
    }
    Value::Object(out)
}

#[test]
fn config_settings_keys_getters_match_ts() {
    let home = Path::new("<home>");
    for case in fixture()["cases"].as_array().unwrap() {
        let settings = case["settings"].as_object().unwrap();
        let label = case["settings"].to_string();
        let text = |v: Option<String>| v.map_or(Value::Null, Value::from);
        assert_eq!(ok(text(keys::shell_path(settings, home))), case["shellPath"], "{label}");
        assert_eq!(
            ok(text(keys::shell_command_prefix(settings))),
            case["shellCommandPrefix"],
            "{label}"
        );
        assert_eq!(
            ok(keys::thinking_budgets(settings).cloned().unwrap_or(Value::Null)),
            case["thinkingBudgets"],
            "{label}"
        );
        let timeout = match keys::websocket_connect_timeout_ms(settings) {
            Ok(v) => ok(v.map_or(Value::Null, |n| {
                serde_json::Number::from_f64(n).map_or(Value::Null, |n| {
                    // JSON.stringify writes integral values without a fraction.
                    match n.as_f64() {
                        Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Value::from(f as i64),
                        _ => Value::Number(n),
                    }
                })
            })),
            Err(e) => json!({ "error": e }),
        };
        assert_eq!(timeout, case["websocketConnectTimeoutMs"], "{label}");
        assert_eq!(
            ok(overrides_json(keys::terminal_capability_overrides(settings))),
            case["terminalCapabilityOverrides"],
            "{label}"
        );
        assert_eq!(
            ok(keys::code_block_indent(settings).into()),
            case["codeBlockIndent"],
            "{label}"
        );
        // getWarnings returns the object; the one key read from it decides the warning.
        let warnings = &case["warnings"]["value"];
        assert_eq!(
            keys::anthropic_extra_usage_warning(settings),
            warnings["anthropicExtraUsage"] != json!(false),
            "{label}"
        );
        assert_eq!(
            ok(keys::default_tools(settings).map_or(Value::Null, |t| json!(t))),
            case["defaultTools"],
            "{label}"
        );
    }
}

/// `getLastChangelogVersion` reads the merged settings (a project value wins); `setLastChangelogVersion` writes the
/// global file, merged into what it holds, and leaves the project file alone.
#[test]
fn config_settings_keys_last_changelog_version_writes_global() {
    for case in fixture()["writes"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, cwd) = (dir.path().join("agent"), dir.path().join("cwd"));
        fs::create_dir_all(&agent).unwrap();
        fs::create_dir_all(cwd.join(".pi")).unwrap();
        if !case["before"].is_null() {
            fs::write(agent.join("settings.json"), case["before"].to_string()).unwrap();
        }
        fs::write(cwd.join(".pi/settings.json"), r#"{"lastChangelogVersion":"9.9.9"}"#).unwrap();
        let mut manager = SettingsManager::create(&cwd, &agent, true);
        assert_eq!(json!(manager.last_changelog_version()), case["seen"]);
        manager.set_last_changelog_version("1.2.3");
        assert_eq!(json!(manager.last_changelog_version()), case["after"]);
        assert_eq!(
            fs::read_to_string(agent.join("settings.json")).unwrap(),
            case["global"].as_str().unwrap()
        );
        assert_eq!(
            fs::read_to_string(cwd.join(".pi/settings.json")).unwrap(),
            case["project"].as_str().unwrap()
        );
    }
}
