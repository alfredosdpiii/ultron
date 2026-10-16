//! Replays `fixtures/package_checks.json`, recorded from the TypeScript package manager by
//! `fixtures/package_checks.mjs`: the startup update check (`checkForAvailableUpdates`) and the temporary `-e`
//! sources (`resolveExtensionSources(sources, { temporary: true })`). Each case gets the same private temp root:
//! the recorded fake npm (logging its arguments, installing from a local registry file), and the real git against
//! local repositories only (GIT_CONFIG_GLOBAL maps https://github.com/ to file://<TMP>/remotes/, and
//! GIT_ALLOW_PROTOCOL=file refuses any other transport). The result, the error, the npm log, the files left under
//! agent/tmp and the temporary checkout's commit must equal TypeScript's.

use cli::packages::manager::{PackageManager, Scope};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/package_checks.json")).expect("fixture parses")
}

/// The generator's `tree`: entries under `rel` without `.git`, directories private (0700) or not.
fn tree(root: &Path, rel: &str, tmp: &str) -> Vec<Value> {
    use std::os::unix::fs::PermissionsExt;
    let base = root.join(rel);
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &str, tmp: &str, out: &mut Vec<Value>) {
        let mut names: Vec<String> = std::fs::read_dir(base.join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            if name == ".git" {
                continue;
            }
            let p = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            let meta = std::fs::symlink_metadata(base.join(&p)).unwrap();
            if meta.is_dir() {
                let private = meta.permissions().mode() & 0o777 == 0o700;
                out.push(json!({"path": p, "kind": "dir", "private": private}));
                walk(base, &p, tmp, out);
            } else {
                let text = std::fs::read_to_string(base.join(&p)).unwrap().replace(tmp, "<TMP>");
                out.push(json!({"path": p, "kind": "file", "text": text}));
            }
        }
    }
    if base.exists() {
        walk(&base, "", tmp, &mut out);
    }
    out
}

fn sh(root: &Path, command: &str, env: &[(String, String)]) {
    let status = std::process::Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(root)
        .env_clear()
        .envs(env.iter().cloned())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "setup failed: {command}");
}

fn run(case: &Value, fixture: &Value) -> Value {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let tmp = root.display().to_string();
    for d in ["work", "agent", "home", "bin"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let npm = root.join("bin/npm");
    std::fs::write(&npm, fixture["fakeNpm"].as_str().unwrap()).unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join("registry.json"), fixture["registry"].as_str().unwrap()).unwrap();
    std::fs::write(
        root.join("gitconfig"),
        format!(
            "[url \"file://{tmp}/remotes/\"]\n\tinsteadOf = https://github.com/\n[advice]\n\tdetachedHead = false\n[init]\n\tdefaultBranch = main\n"
        ),
    )
    .unwrap();
    std::fs::write(root.join("npm.log"), "").unwrap();
    for (rel, text) in case["files"].as_object().into_iter().flatten() {
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
    let path = format!("{}:/usr/bin:/bin", root.join("bin").display());
    let mut child_env: Vec<(String, String)> = vec![
        ("FAKE_NPM_LOG".into(), root.join("npm.log").display().to_string()),
        (
            "FAKE_NPM_REGISTRY".into(),
            root.join("registry.json").display().to_string(),
        ),
        ("GIT_CONFIG_GLOBAL".into(), root.join("gitconfig").display().to_string()),
    ];
    for (k, v) in fixture["gitEnv"].as_object().unwrap() {
        child_env.push((k.clone(), v.as_str().unwrap().to_string()));
    }
    let mut setup_env = child_env.clone();
    setup_env.push(("PATH".into(), path.clone()));
    setup_env.push(("HOME".into(), root.join("home").display().to_string()));
    for command in case["setup"].as_array().into_iter().flatten() {
        sh(&root, command.as_str().unwrap(), &setup_env);
    }
    let mut vars: HashMap<String, String> = [
        ("PATH".to_string(), path),
        (
            "ULTRON_CODING_AGENT_DIR".to_string(),
            root.join("agent").display().to_string(),
        ),
    ]
    .into();
    for (k, v) in case["env"].as_object().into_iter().flatten() {
        vars.insert(k.clone(), v.as_str().unwrap().to_string());
    }
    let env = cli::Env {
        cwd: root.join("work"),
        home: root.join("home"),
        var: Arc::new(move |k| vars.get(k).cloned()),
        docs: root.join("docs"),
    };
    let mut settings = config::settings::SettingsManager::create(&root.join("work"), &root.join("agent"), false);
    settings.set_project_trusted(case["trust"] == true);
    let mut manager = PackageManager::new(&env, settings);
    manager.child_env = child_env;
    manager.child_stdout_to_stderr = true;
    let sub = |s: &str| s.replace(&tmp, "<TMP>");
    let (result, error) = if case["op"] == "check" {
        match manager.check_for_available_updates() {
            Ok(updates) => (
                json!(
                    updates
                        .iter()
                        .map(|u| json!({
                            "source": u.source,
                            "displayName": u.display_name,
                            "type": u.kind,
                            "scope": if u.scope == Scope::Project { "project" } else { "user" },
                        }))
                        .collect::<Vec<_>>()
                ),
                Value::Null,
            ),
            Err(e) => (Value::Null, json!(sub(&e))),
        }
    } else {
        let sources: Vec<String> = case["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        match manager.resolve_extension_sources(&sources) {
            Ok(r) => {
                let list = |l: &[String]| json!(l.iter().map(|p| sub(p)).collect::<Vec<_>>());
                (
                    json!({"extensions": list(&r.extensions), "skills": list(&r.skills), "prompts": list(&r.prompts), "themes": list(&r.themes)}),
                    Value::Null,
                )
            }
            Err(e) => (Value::Null, json!(sub(&e))),
        }
    };
    let cwd = std::env::current_dir().unwrap().display().to_string();
    let mut npm_log: Vec<String> = std::fs::read_to_string(root.join("npm.log"))
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| sub(&l.replace(&format!("[{cwd}]"), "[<CWD>]")))
        .collect();
    npm_log.sort();
    // `getExtensionTempFolder` creates every missing directory 0700 (`mkdirSync(..., { recursive, mode: 0o700 })`); one the setup made
    // keeps its mode.
    let setup_made_it = case["setup"].to_string().contains("agent/tmp");
    if let Ok(meta) = std::fs::metadata(root.join("agent/tmp"))
        && !setup_made_it
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o700, "{}: agent/tmp", case["name"]);
    }
    let mut subjects = serde_json::Map::new();
    // The temporary checkout of github.com/test/ext (`getTemporaryDir`).
    let checkout = "agent/tmp/extensions/git-github.com/5896401d/test/ext";
    if root.join(checkout).join(".git").exists() {
        let out = std::process::Command::new("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(root.join(checkout))
            .env_clear()
            .envs(setup_env.iter().cloned())
            .output()
            .unwrap();
        subjects.insert(checkout.into(), json!(String::from_utf8_lossy(&out.stdout).trim()));
    }
    json!({"result": result, "error": error, "npmLog": npm_log, "tmpTree": tree(&root, "agent/tmp", &tmp), "subjects": subjects})
}

#[test]
fn cli_packages_update_check_and_temporary_sources_match_typescript() {
    let fixture = fixture();
    let mut diffs = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let got = run(case, &fixture);
        let mut want_tree = case["tmpTree"].clone();
        for entry in want_tree.as_array_mut().unwrap() {
            if let Some(mode) = entry.as_object_mut().unwrap().shift_remove("mode") {
                entry["private"] = json!(mode == "700");
            }
        }
        let want = json!({"result": case["result"], "error": case["error"], "npmLog": case["npmLog"], "tmpTree": want_tree, "subjects": case["subjects"]});
        for key in ["result", "error", "npmLog", "tmpTree", "subjects"] {
            if got[key] != want[key] {
                diffs.push(format!(
                    "{}: {key}\n  rust {}\n  ts   {}",
                    case["name"].as_str().unwrap(),
                    got[key],
                    want[key]
                ));
            }
        }
    }
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
}
