//! Local resource resolution and the helpers it and the self-update use, against answers recorded from TypeScript by
//! crates/cli/fixtures/packages.mjs (`units`): node-semver's `valid` and `gt`, minimatch, `ignore`, and
//! DefaultPackageManager.resolve() over directory trees. Pi packages are not supported by ultron-rs: the settings
//! `packages` of the recorded trees are ignored, so the local resolution is compared with TypeScript's top-level
//! resources (a package resource ranks after every top-level one, so leaving them out changes nothing else).

use resources::ignore::Ignore;
use serde_json::{Value, json};
use session_core::local_resources::{LocalResources, RESOURCE_TYPES, glob};
use std::collections::HashMap;
use std::sync::Arc;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/packages.json")).expect("fixture parses")
}

#[test]
fn cli_local_resources_semver_matches_node_semver() {
    let f = fixture();
    let s = &f["units"]["semver"];
    let mut diffs = Vec::new();
    for c in s["valid"].as_array().unwrap() {
        let got = cli::semver::valid(c[0].as_str().unwrap());
        if got.as_deref() != c[1].as_str() {
            diffs.push(format!("valid {c}: {got:?}"));
        }
    }
    for c in s["gt"].as_array().unwrap() {
        let got = cli::semver::compare(c[0].as_str().unwrap(), c[1].as_str().unwrap())
            .map(|o| o == std::cmp::Ordering::Greater);
        if got.map(Value::Bool).as_ref() != Some(&c[2]) {
            diffs.push(format!("gt {c}: {got:?}"));
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

#[test]
fn cli_local_resources_minimatch_matches_typescript() {
    let mut diffs = Vec::new();
    for c in fixture()["units"]["minimatch"].as_array().unwrap() {
        let got = glob::minimatch(c[0].as_str().unwrap(), c[1].as_str().unwrap());
        if json!(got) != c[2] {
            diffs.push(format!("{c}: {got}"));
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

#[test]
fn cli_local_resources_ignore_rules_match_the_ignore_package() {
    let f = fixture();
    let paths: Vec<&str> = f["units"]["ignore"]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    let mut diffs = Vec::new();
    for c in f["units"]["ignore"]["cases"].as_array().unwrap() {
        let rules: Vec<&str> = c[0].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
        let mut ig = Ignore::new();
        ig.add(&rules);
        for (path, want) in paths.iter().zip(c[1].as_array().unwrap()) {
            if json!(ig.ignores(path)) != *want {
                diffs.push(format!("{rules:?} {path}: ts {want}"));
            }
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

#[test]
fn cli_local_resources_resolve_matches_typescript_top_level_resources() {
    let mut diffs = Vec::new();
    let mut compared = 0;
    for case in fixture()["units"]["resolve"].as_array().unwrap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let tmp_text = root.display().to_string();
        for d in ["agent", "work", "home"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for (rel, text) in case["files"].as_object().unwrap() {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text.as_str().unwrap()).unwrap();
        }
        let global = if case["global"].is_null() {
            json!({})
        } else {
            case["global"].clone()
        };
        std::fs::write(root.join("agent/settings.json"), global.to_string()).unwrap();
        if !case["project"].is_null() {
            std::fs::create_dir_all(root.join("work/.pi")).unwrap();
            std::fs::write(root.join("work/.pi/settings.json"), case["project"].to_string()).unwrap();
        }
        let vars: HashMap<String, String> = [
            ("PI_OFFLINE", "1".to_string()),
            ("ULTRON_CODING_AGENT_DIR", root.join("agent").display().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let env = cli::Env {
            cwd: root.join("work"),
            home: root.join("home"),
            var: Arc::new(move |k| vars.get(k).cloned()),
            docs: root.join("docs"),
        };
        let mut settings = config::settings::SettingsManager::create(&root.join("work"), &root.join("agent"), false);
        settings.set_project_trusted(case["trust"] == true);
        let resolved = LocalResources::new(&env, settings).resolve();
        let sub = |s: &str| s.replace(&tmp_text, "<TMP>");
        for kind in RESOURCE_TYPES {
            let got: Vec<Value> = resolved
                .of(kind)
                .iter()
                .map(|r| {
                    json!({
                        "path": sub(&r.path),
                        "enabled": r.enabled,
                        "source": r.metadata.source,
                        "scope": r.metadata.scope.name(),
                        "origin": "top-level",
                        "baseDir": r.metadata.base_dir.as_deref().map(sub),
                    })
                })
                .collect();
            let want: Vec<Value> = case["resolved"][kind.key()]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["origin"] == "top-level")
                .cloned()
                .collect();
            compared += want.len();
            if got != want {
                diffs.push(format!(
                    "{} {}:\nrust {}\nts   {}",
                    case["name"],
                    kind.key(),
                    serde_json::to_string_pretty(&got).unwrap(),
                    serde_json::to_string_pretty(&want).unwrap()
                ));
            }
        }
    }
    assert!(compared > 0, "the recorded trees have top-level resources");
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

/// The Pi extensions TypeScript resolves at top level (`<agentDir>/extensions/` conventions and manifests, the
/// trusted project's `.pi/extensions/`, the settings `extensions` paths and filters) are the ones ultron-rs names
/// as ignored: replays the recorded `resolve()` trees, comparing each top-level extension and its `enabled`.
#[test]
fn cli_local_resources_extensions_match_typescript_top_level() {
    let f = fixture();
    let mut diffs = Vec::new();
    let mut compared = 0;
    for (i, case) in f["units"]["resolve"].as_array().unwrap().iter().enumerate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let tmp_text = root.display().to_string();
        for d in ["agent", "work", "home"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for (rel, text) in case["files"].as_object().unwrap() {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text.as_str().unwrap()).unwrap();
        }
        let global = if case["global"].is_null() {
            json!({})
        } else {
            case["global"].clone()
        };
        std::fs::write(root.join("agent/settings.json"), global.to_string()).unwrap();
        if !case["project"].is_null() {
            std::fs::create_dir_all(root.join("work/.pi")).unwrap();
            std::fs::write(root.join("work/.pi/settings.json"), case["project"].to_string()).unwrap();
        }
        let vars: HashMap<String, String> = HashMap::from([(
            "ULTRON_CODING_AGENT_DIR".to_string(),
            root.join("agent").display().to_string(),
        )]);
        let env = cli::Env {
            cwd: root.join("work"),
            home: root.join("home"),
            var: Arc::new(move |k| vars.get(k).cloned()),
            docs: root.join("docs"),
        };
        let mut settings = config::settings::SettingsManager::create(&root.join("work"), &root.join("agent"), false);
        settings.set_project_trusted(case["trust"] == true);
        let resources = LocalResources::new(&env, settings);
        let sub = |s: &str| s.replace(&tmp_text, "<TMP>");
        let got: Vec<Value> = resources
            .extensions()
            .iter()
            .map(|r| {
                json!({
                    "path": sub(&r.path),
                    "enabled": r.enabled,
                    "source": r.metadata.source,
                    "scope": r.metadata.scope.name(),
                    "origin": "top-level",
                    "baseDir": r.metadata.base_dir.as_deref().map(sub),
                })
            })
            .collect();
        let want: Vec<Value> = case["resolved"]["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["origin"] == "top-level")
            .cloned()
            .collect();
        compared += want.len();
        if got != want {
            diffs.push(format!(
                "{i} {}:\nrust {}\nts   {}",
                case["name"],
                serde_json::to_string_pretty(&got).unwrap(),
                serde_json::to_string_pretty(&want).unwrap()
            ));
        }
        // The note names the enabled ones.
        let note = session_core::local_resources::extensions::ignored_extensions_note(&resources).map(|n| sub(&n));
        let enabled: Vec<&str> = want
            .iter()
            .filter(|r| r["enabled"] == true)
            .map(|r| r["path"].as_str().unwrap())
            .collect();
        let expected = (!enabled.is_empty()).then(|| {
            format!(
                "Pi extensions are not supported by ultron-rs; ignoring {} extension{}: {}",
                enabled.len(),
                if enabled.len() == 1 { "" } else { "s" },
                enabled.join(", ")
            )
        });
        assert_eq!(note, expected, "{}", case["name"]);
    }
    assert_eq!(compared, 6, "the recorded trees' top-level extensions");
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}
