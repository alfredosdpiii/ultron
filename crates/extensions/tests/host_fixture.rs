//! Replays fixtures/host.json (recorded from the TypeScript product by fixtures/generate-host.mjs): minimatch as the
//! package manager calls it, settings `extensions` patterns over discovered extensions (`DefaultPackageManager`),
//! `getShortcuts` conflict rules, and `registerFlag` / `getFlag` / `getFlagValues` over real extension processes.

use extensions::{ExtensionHost, NoHost, SessionInfo, discover, parse_manifest};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/host.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn probe() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/probe.py")
}

#[test]
fn extensions_host_minimatch_matches_typescript() {
    let fixture = fixture();
    let cases = fixture["globs"].as_array().unwrap();
    assert!(cases.len() >= 25);
    for case in cases {
        let (text, pattern) = (case["text"].as_str().unwrap(), case["pattern"].as_str().unwrap());
        assert_eq!(
            extensions::glob::minimatch(text, pattern),
            case["matches"].as_bool().unwrap(),
            "minimatch({text:?}, {pattern:?})"
        );
    }
}

#[test]
fn extensions_host_settings_patterns_match_the_package_manager() {
    let fixture = fixture();
    for scenario in fixture["patterns"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (agent, cwd) = (root.join("agent"), root.join("cwd"));
        std::fs::create_dir_all(cwd.join(".pi")).unwrap();
        for dir in scenario["dirs"].as_array().unwrap() {
            let dir = root.join(dir.as_str().unwrap());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("extension.json"), r#"{"command":"python3"}"#).unwrap();
        }
        let entries = |key: &str| -> Map<String, Value> {
            let list: Vec<String> = scenario[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    e.as_str()
                        .unwrap()
                        .replace("<TMP>", &root.to_string_lossy())
                        .replace("{entry}", "extension.json")
                })
                .collect();
            json!({ "extensions": list }).as_object().unwrap().clone()
        };
        let found = discover(&cwd, &agent, &root, &entries("user"), &entries("project"), true);
        assert!(found.diagnostics.is_empty(), "{name}: {:?}", found.diagnostics);
        let rel = |path: &Path| {
            path.parent()
                .unwrap()
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let enabled: Vec<String> = found.manifests.iter().map(|m| rel(&m.path)).collect();
        let disabled: Vec<String> = found.disabled.iter().map(|p| rel(p)).collect();
        let expected = |on: bool| -> Vec<String> {
            scenario["resolved"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["enabled"] == on)
                .map(|r| r["dir"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(enabled, expected(true), "{name}: enabled");
        assert_eq!(disabled, expected(false), "{name}: disabled");
    }
}

#[test]
fn extensions_host_shortcut_conflicts_match_typescript() {
    let fixture = fixture();
    for scenario in fixture["shortcuts"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let extensions: Vec<extensions::shortcuts::Registered> = scenario["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let path = e["path"].as_str().unwrap().to_string();
                let keys = e["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| {
                        let key = k.as_str().unwrap().to_string();
                        let description = format!("{path} {key}");
                        (key, Some(description))
                    })
                    .collect();
                (path, keys)
            })
            .collect();
        let keybindings: Vec<(String, Vec<String>)> = scenario["keybindings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                let keys = pair[1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| k.as_str().unwrap().to_string())
                    .collect();
                (pair[0].as_str().unwrap().to_string(), keys)
            })
            .collect();
        let resolved = extensions::shortcuts::resolve(&extensions, &keybindings);
        let shortcuts: Vec<Value> = resolved
            .shortcuts
            .iter()
            .map(|(key, s)| {
                json!({ "key": key, "registered": s.key, "description": s.description, "extensionPath": s.extension_path })
            })
            .collect();
        assert_eq!(Value::Array(shortcuts), scenario["shortcuts"], "{name}: shortcuts");
        let diagnostics: Vec<Value> = resolved
            .diagnostics
            .iter()
            .map(|(message, path)| json!({ "message": message, "path": path }))
            .collect();
        assert_eq!(
            Value::Array(diagnostics),
            scenario["diagnostics"],
            "{name}: diagnostics"
        );
    }
}

/// A probe extension named `name` registering `flags`, with the `host` function.
fn flag_extension(dir: &Path, name: &str, flags: &Value) -> extensions::Manifest {
    let ext_dir = dir.join(name);
    std::fs::create_dir_all(&ext_dir).unwrap();
    let capabilities = json!({ "flags": flags, "functions": [{ "name": "host" }] });
    let manifest = json!({
        "name": name,
        "command": python(),
        "args": ["-I", probe().to_string_lossy()],
        "env": { "PROBE_CAPABILITIES": capabilities.to_string() },
        "startupTimeoutMs": 120_000,
        "eventTimeoutMs": 120_000,
    });
    let path = ext_dir.join("extension.json");
    std::fs::write(&path, manifest.to_string()).unwrap();
    parse_manifest(&path, &manifest.to_string()).unwrap()
}

fn session(flags: Map<String, Value>) -> SessionInfo {
    SessionInfo {
        cwd: "/w".into(),
        mode: "print".into(),
        has_ui: false,
        flags,
    }
}

const CALL: Option<Duration> = Some(Duration::from_secs(120));

#[test]
fn extensions_host_flags_match_typescript() {
    let fixture = fixture();
    for scenario in fixture["flags"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let manifests: Vec<_> = scenario["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| flag_extension(dir.path(), e["path"].as_str().unwrap(), &e["flags"]))
            .collect();
        let host = ExtensionHost::load(manifests, session(Map::new()), Arc::new(NoHost));
        let loaded: Vec<&str> = host.processes().iter().map(|p| p.name()).collect();
        assert_eq!(json!(loaded), scenario["loaded"], "{name}: loaded");
        let errors = scenario["errors"].as_array().unwrap();
        assert_eq!(host.diagnostics.len(), errors.len(), "{name}: {:?}", host.diagnostics);
        for (diagnostic, error) in host.diagnostics.iter().zip(errors) {
            let path = error["path"].as_str().unwrap();
            assert!(diagnostic.path.ends_with(format!("{path}/extension.json")), "{name}");
            assert!(
                diagnostic.message.ends_with(error["error"].as_str().unwrap()),
                "{name}: {}",
                diagnostic.message
            );
        }
        for view in scenario["views"].as_array().unwrap() {
            let extension = view["path"].as_str().unwrap();
            for (flag, expected) in view["flags"].as_object().unwrap() {
                let answer = host
                    .call_function(
                        extension,
                        "host",
                        json!({ "method": "flags/get", "params": { "name": flag } }),
                        CALL,
                        None,
                    )
                    .unwrap();
                assert_eq!(answer["details"], *expected, "{name}: {extension} getFlag({flag})");
            }
        }
        assert_eq!(Value::Object(host.flag_values()), scenario["values"], "{name}: values");
        let registered: Vec<Value> = host
            .flags()
            .into_iter()
            .map(|(flag, path)| {
                let ext = Path::new(&path)
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                json!({ "name": flag.name, "type": flag.kind, "extensionPath": ext })
            })
            .collect();
        assert_eq!(Value::Array(registered), scenario["registered"], "{name}: getFlags");
        host.shutdown();
    }
}

/// `applyExtensionFlags` (agent-session-services.ts, not exported: ported with its literal messages): command-line
/// values for registered flags override defaults, a boolean flag is `true` whatever was given, a string flag without
/// a value and unknown options are reported. A restart's `initialize.flags` carries the session's values.
#[test]
fn extensions_host_command_line_flags_apply_like_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let manifests = vec![flag_extension(
        dir.path(),
        "a",
        &json!([
            { "name": "plan", "type": "boolean", "default": false },
            { "name": "mode", "type": "string", "default": "fast" },
            { "name": "depth", "type": "string" },
        ]),
    )];
    let cli: Map<String, Value> = json!({ "plan": "ignored", "mode": "slow", "depth": true, "zzz": true, "yyy": "v" })
        .as_object()
        .unwrap()
        .clone();
    let host = ExtensionHost::load(manifests, session(cli), Arc::new(NoHost));
    assert!(host.diagnostics.is_empty(), "{:?}", host.diagnostics);
    assert_eq!(
        host.flag_diagnostics,
        [
            "Extension flag \"--depth\" requires a value",
            "Unknown options: --zzz, --yyy"
        ]
    );
    assert_eq!(
        Value::Object(host.flag_values()),
        json!({ "plan": true, "mode": "slow" })
    );
    let get = |flag: &str| {
        host.call_function(
            "a",
            "host",
            json!({ "method": "flags/get", "params": { "name": flag } }),
            CALL,
            None,
        )
        .unwrap()["details"]
            .clone()
    };
    assert_eq!(
        (get("plan"), get("mode"), get("depth")),
        (json!(true), json!("slow"), Value::Null)
    );
    host.set_flag_value("depth", json!("deep"));
    assert_eq!(get("depth"), json!("deep"));
    host.shutdown();
}
