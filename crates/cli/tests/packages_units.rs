//! The package manager's parsers and resource resolution against answers recorded from TypeScript by
//! crates/cli/fixtures/packages.mjs (`units`): parseGitUrl, npm sources, node-semver, minimatch, `ignore`, and
//! DefaultPackageManager.resolve() over directory trees.

use cli::packages::git_url::parse_git_url;
use cli::packages::manager::{Origin, PackageManager, Parsed, Scope, parse_source};
use cli::packages::{glob, ignore::Ignore, semver};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/packages.json")).expect("fixture parses")
}

#[test]
fn cli_packages_parse_git_url_matches_typescript() {
    let mut diffs = Vec::new();
    for case in fixture()["units"]["git"].as_array().unwrap() {
        let source = case[0].as_str().unwrap();
        let got = parse_git_url(source).map(|g| {
            let mut v = json!({"type": "git", "repo": g.repo, "host": g.host, "path": g.path, "pinned": g.pinned});
            if let Some(r) = g.git_ref {
                v["ref"] = json!(r);
            }
            v
        });
        let want = &case[1];
        let want = if want.is_null() {
            None
        } else {
            let mut w = want.clone();
            if w["ref"].is_null() {
                w.as_object_mut().unwrap().remove("ref");
            }
            Some(w)
        };
        if got != want {
            diffs.push(format!("{source:?}: rust {got:?}, ts {want:?}"));
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

#[test]
fn cli_packages_parse_npm_source_matches_typescript() {
    for case in fixture()["units"]["npm"].as_array().unwrap() {
        let Parsed::Npm(n) = parse_source(case[0].as_str().unwrap()) else {
            panic!("not npm: {case}");
        };
        let want = &case[1];
        assert_eq!(n.spec, want["spec"], "{case}");
        assert_eq!(n.name, want["name"], "{case}");
        assert_eq!(n.version.as_deref(), want["version"].as_str(), "{case}");
        assert_eq!(n.range.is_some(), want["range"].is_string(), "{case}");
        assert_eq!(json!(n.pinned), want["pinned"], "{case}");
    }
}

#[test]
fn cli_packages_semver_matches_node_semver() {
    let f = fixture();
    let s = &f["units"]["semver"];
    let mut diffs = Vec::new();
    for c in s["valid"].as_array().unwrap() {
        let got = semver::valid(c[0].as_str().unwrap());
        if got.as_deref() != c[1].as_str() {
            diffs.push(format!("valid {c}: {got:?}"));
        }
    }
    for c in s["validRange"].as_array().unwrap() {
        let got = semver::valid_range(c[0].as_str().unwrap());
        if json!(got) != c[1] {
            diffs.push(format!("validRange {c}: {got}"));
        }
    }
    for c in s["satisfies"].as_array().unwrap() {
        let got = semver::satisfies(c[0].as_str().unwrap(), c[1].as_str().unwrap());
        if json!(got) != c[2] {
            diffs.push(format!("satisfies {c}: {got}"));
        }
    }
    let pool = [
        "1.0.0",
        "1.2.3",
        "1.2.4-beta.1",
        "1.3.0",
        "2.0.0",
        "2.1.0",
        "0.3.0",
        "0.0.4",
        "bogus",
    ];
    for c in s["maxSatisfying"].as_array().unwrap() {
        let got = semver::max_satisfying(&pool, c[0].as_str().unwrap());
        if got != c[1].as_str() {
            diffs.push(format!("maxSatisfying {c}: {got:?}"));
        }
    }
    for c in s["gt"].as_array().unwrap() {
        let got = semver::gt(c[0].as_str().unwrap(), c[1].as_str().unwrap());
        if got.map(Value::Bool).as_ref() != Some(&c[2]) {
            diffs.push(format!("gt {c}: {got:?}"));
        }
    }
    for c in s["highest"].as_array().unwrap() {
        let list: Vec<&str> = c[0].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        if semver::highest(&list) != c[1].as_str() {
            diffs.push(format!("highest {c}"));
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

#[test]
fn cli_packages_minimatch_matches_typescript() {
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
fn cli_packages_ignore_rules_match_the_ignore_package() {
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

fn scope_name(scope: Scope) -> &'static str {
    scope.name()
}

#[test]
fn cli_packages_resolve_matches_typescript() {
    let mut diffs = Vec::new();
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
        let resolved = PackageManager::new(&env, settings).resolve().unwrap();
        let sub = |s: &str| s.replace(&tmp_text, "<TMP>");
        for kind in cli::packages::resources::RESOURCE_TYPES {
            let got: Vec<Value> = resolved
                .of(kind)
                .iter()
                .map(|r| {
                    json!({
                        "path": sub(&r.path),
                        "enabled": r.enabled,
                        "source": r.metadata.source,
                        "scope": scope_name(r.metadata.scope),
                        "origin": if r.metadata.origin == Origin::Package { "package" } else { "top-level" },
                        "baseDir": r.metadata.base_dir.as_deref().map(sub),
                    })
                })
                .collect();
            let want = case["resolved"][kind.key()].as_array().unwrap();
            if got != *want {
                diffs.push(format!(
                    "{} {}:\nrust {}\nts   {}",
                    case["name"],
                    kind.key(),
                    serde_json::to_string_pretty(&got).unwrap(),
                    serde_json::to_string_pretty(want).unwrap()
                ));
            }
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}
