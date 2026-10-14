//! Replays crates/config/fixtures/config.json (recorded from the TypeScript product by fixtures/generate.mjs)
//! against the Rust implementation and requires identical API results and file bytes.

use std::fs;
use std::path::{Path, PathBuf};

use config::auth::{AuthStorage, read_stored_credential, resolve_config_value, validate_auth_file};
use config::models_store::ModelsStore;
use config::paths::{AgentPaths, ENV_AGENT_DIR, agent_dir};
use config::settings::{Scope, SettingsManager};
use config::trust::{ProjectTrustStore, TrustUpdate, has_trust_requiring_project_resources, trust_options};
use serde_json::{Value, json};

fn fixture() -> Value {
    let text = include_str!("../fixtures/config.json");
    serde_json::from_str(text).expect("fixture parses")
}

struct Root {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn root() -> Root {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = fs::canonicalize(dir.path()).expect("canonical tempdir");
    Root { _dir: dir, path }
}

impl Root {
    fn rel(&self, value: Value) -> Value {
        let text = value.to_string().replace(&*self.path.to_string_lossy(), "<root>");
        serde_json::from_str(&text).expect("still json")
    }
    fn read(&self, path: &Path) -> Value {
        match fs::read_to_string(path) {
            Ok(text) => Value::String(text.replace(&*self.path.to_string_lossy(), "<root>")),
            Err(_) => Value::Null,
        }
    }
}

fn patch(step: &Value) -> Vec<(String, Option<Value>)> {
    step["patch"]
        .as_array()
        .expect("patch")
        .iter()
        .map(|entry| (entry[0].as_str().expect("key").to_string(), entry.get(1).cloned()))
        .collect()
}

fn borrowed(patch: &[(String, Option<Value>)]) -> Vec<(&str, Option<Value>)> {
    patch.iter().map(|(k, v)| (k.as_str(), v.clone())).collect()
}

fn opt(value: Option<impl Into<Value>>) -> Value {
    value.map_or(Value::Null, Into::into)
}

fn view(m: &mut SettingsManager, home: &Path, cwd: &Path) -> Value {
    let errors: Vec<Value> = m
        .drain_errors()
        .into_iter()
        .map(|e| json!({ "scope": e.scope.name(), "path": e.path.to_string_lossy() }))
        .collect();
    json!({
        "settings": m.settings(), "global": m.global_settings(), "project": m.project_settings(),
        "trusted": m.is_project_trusted(), "theme": opt(m.theme()), "themeSetting": opt(m.theme_setting()),
        "steeringMode": m.steering_mode(), "followUpMode": m.follow_up_mode(), "transport": m.transport(),
        "compactionEnabled": m.compaction_enabled(), "retry": m.retry_settings(), "branchSummary": m.branch_summary_settings(),
        "sessionDir": opt(m.session_dir(home).map(|p| p.to_string_lossy().into_owned())),
        "defaultProjectTrust": m.default_project_trust(), "autoreview": m.autoreview_settings(), "loki": m.loki_settings(),
        "defaultProvider": opt(m.default_provider()), "defaultModel": opt(m.default_model()), "errors": errors,
        "projectDirExists": cwd.join(".pi").exists(),
    })
}

fn run_step(m: &mut SettingsManager, step: &Value, home: &Path, cwd: &Path) -> Value {
    let s = |key: &str| step[key].as_str().expect("string arg").to_string();
    let value = || step.get("value").cloned();
    match step["op"].as_str().expect("op") {
        "view" => return view(m, home, cwd),
        "reload" => m.reload(),
        "setAutoreviewSettings" => m.set_autoreview_settings(&borrowed(&patch(step))),
        "setLokiSettings" => m.set_loki_settings(&borrowed(&patch(step))),
        "setDefaultModelAndProvider" => {
            m.set_global("defaultProvider", Some(s("provider").into()));
            m.set_global("defaultModel", Some(s("model").into()));
        }
        "setModelThinkingLevel" => m.set_model_thinking_level(&s("provider"), &s("model"), &s("value")),
        "removeModelThinkingLevel" => m.remove_model_thinking_level(&s("provider"), &s("model")),
        "setProjectTrusted" => m.set_project_trusted(step["value"].as_bool().expect("bool")),
        "setProjectPackages" => {
            if let Err(e) = m.set_project("packages", value()) {
                return json!({ "error": e });
            }
        }
        "setCompactionEnabled" => m.set_global_nested("compaction", "enabled", value()),
        "setRetryEnabled" => m.set_global_nested("retry", "enabled", value()),
        op => {
            let field = match op {
                "setTheme" => "theme",
                "setSteeringMode" => "steeringMode",
                "setDefaultThinkingLevel" => "defaultThinkingLevel",
                "setExtensionPaths" => "extensions",
                "setDefaultProjectTrust" => "defaultProjectTrust",
                other => panic!("unmapped settings op {other}"),
            };
            m.set_global(field, value());
        }
    }
    Value::Null
}

#[test]
fn config_settings_scenarios_match_typescript() {
    let fx = fixture();
    let root = root();
    let home = root.path.join("fakehome");
    for (index, scenario) in fx["settings"].as_array().expect("settings").iter().enumerate() {
        let base = root.path.join(format!("settings-{index}"));
        let (agent, cwd) = (base.join("agent"), base.join("project"));
        fs::create_dir_all(&agent).expect("agent dir");
        fs::create_dir_all(&cwd).expect("project dir");
        let global = agent.join("settings.json");
        let project = cwd.join(".pi").join("settings.json");
        let mut managers: Vec<(String, SettingsManager)> = Vec::new();
        for (n, step) in scenario["steps"].as_array().expect("steps").iter().enumerate() {
            let name = step["name"].as_str().unwrap_or_default().to_string();
            let result = match step["op"].as_str().expect("op") {
                "write" => {
                    let path = if step["file"] == "global" { &global } else { &project };
                    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
                    fs::write(path, step["text"].as_str().expect("text")).expect("write");
                    Value::Null
                }
                "create" => {
                    let trusted = step.get("trusted").and_then(Value::as_bool).unwrap_or(true);
                    managers.push((name, SettingsManager::create(&cwd, &agent, trusted)));
                    Value::Null
                }
                _ => {
                    let m = &mut managers.iter_mut().find(|(n, _)| *n == name).expect("manager").1;
                    run_step(m, step, &home, &cwd)
                }
            };
            let what = format!("{} / step {n} {}", scenario["name"], step["op"]);
            assert_eq!(root.rel(result), step["result"], "result of {what}");
            assert_eq!(root.read(&global), step["files"]["global"], "global file after {what}");
            assert_eq!(
                root.read(&project),
                step["files"]["project"],
                "project file after {what}"
            );
        }
    }
}

#[test]
fn config_settings_concurrent_writers_keep_every_key() {
    let fx = fixture();
    let race = &fx["race"];
    let writes = race["writes"].as_u64().expect("writes");
    let root = root();
    let (agent, cwd) = (root.path.join("agent"), root.path.join("project"));
    fs::create_dir_all(&agent).expect("agent");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::write(agent.join("settings.json"), r#"{"autoreview":{"model":"p/m"}}"#).expect("seed");
    let workers: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|tag| {
            let (agent, cwd) = (agent.clone(), cwd.clone());
            std::thread::spawn(move || {
                let mut m = SettingsManager::create(&cwd, &agent, true);
                for i in 0..writes {
                    m.set_autoreview_settings(&[(&format!("{tag}{i}"), Some(i.into()))]);
                }
                m.drain_errors()
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().expect("worker"), vec![]);
    }
    let text = fs::read_to_string(agent.join("settings.json")).expect("settings");
    let final_settings: Value = serde_json::from_str(&text).expect("json");
    let mut keys: Vec<Value> = final_settings["autoreview"]
        .as_object()
        .expect("autoreview")
        .keys()
        .map(|k| Value::from(k.as_str()))
        .collect();
    keys.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    assert_eq!(Value::Array(keys), race["keys"]);
    assert_eq!(
        agent.join("settings.json.lock").exists(),
        race["lockLeft"].as_bool().expect("bool")
    );
}

#[test]
fn config_settings_load_errors_carry_scope_and_path() {
    let root = root();
    let (agent, cwd) = (root.path.join("agent"), root.path.join("project"));
    fs::create_dir_all(cwd.join(".pi")).expect("dirs");
    fs::create_dir_all(&agent).expect("dirs");
    fs::write(cwd.join(".pi/settings.json"), "[1]").expect("write");
    let mut m = SettingsManager::create(&cwd, &agent, true);
    let errors = m.drain_errors();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].scope, Scope::Project);
    assert_eq!(errors[0].path, cwd.join(".pi/settings.json"));
    assert!(m.drain_errors().is_empty());
    assert!(m.set_project("theme", Some("x".into())).is_ok());
    assert_eq!(
        fs::read_to_string(cwd.join(".pi/settings.json")).expect("unchanged"),
        "[1]"
    );
}

#[test]
fn config_auth_operations_match_typescript() {
    let fx = fixture();
    let auth = &fx["auth"];
    let root = root();
    let path = root.path.join("auth").join("nested").join("auth.json");
    let mut storage = AuthStorage::create(&path);
    for (n, step) in auth["steps"].as_array().expect("steps").iter().enumerate() {
        let provider = step["provider"].as_str().unwrap_or_default();
        let result = match step["op"].as_str().expect("op") {
            "write" => {
                fs::write(&path, step["text"].as_str().expect("text")).expect("write");
                Ok(Value::Null)
            }
            "list" => storage.list().map(|l| {
                l.into_iter()
                    .map(|(id, t)| json!({ "providerId": id, "type": t }))
                    .collect::<Vec<_>>()
                    .into()
            }),
            "read" => storage.read(provider).map(|c| c.unwrap_or(Value::Null)),
            "modify" => storage
                .modify(provider, |_| step.get("value").cloned())
                .map(|c| c.unwrap_or(Value::Null)),
            "delete" => storage.delete(provider).map(|()| Value::Null),
            other => panic!("unmapped auth op {other}"),
        };
        let result = result.unwrap_or_else(|_| json!({ "error": true }));
        let what = format!("auth step {n} {}", step["op"]);
        assert_eq!(root.rel(result), step["result"], "{what}");
        assert_eq!(root.read(&path), step["file"], "file after {what}");
        assert!(!path.with_extension("json.lock").exists(), "lock left after {what}");
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| format!("{:o}", fs::metadata(p).expect("stat").permissions().mode() & 0o777);
    assert_eq!(mode(&path), auth["mode"].as_str().expect("mode"));
    assert_eq!(
        mode(path.parent().expect("dir")),
        auth["dirMode"].as_str().expect("mode")
    );
}

#[test]
fn config_auth_file_validation_matches_typescript() {
    let fx = fixture();
    let root = root();
    for case in fx["validation"].as_array().expect("validation") {
        let text = case["text"].as_str().expect("text");
        let result = match validate_auth_file(text) {
            Ok(data) => {
                json!({ "ok": data.iter().map(|(id, c)| json!({ "providerId": id, "type": c["type"] })).collect::<Vec<_>>() })
            }
            Err(e) => json!({ "error": e }),
        };
        assert_eq!(result, case["result"], "validation of {text}");
        let path = root.path.join("file.json");
        fs::write(&path, text).expect("write");
        assert_eq!(
            read_stored_credential(&path, "a").unwrap_or(Value::Null),
            case["stored"],
            "stored a in {text}"
        );
    }
}

#[test]
fn config_value_templates_match_typescript() {
    let fx = fixture();
    let env = json!({ "A": "1", "B": "two", "A_B9": "ab", "Bpost": "" });
    let lookup = |name: &str| env.get(name).and_then(Value::as_str).map(str::to_string);
    for case in fx["templates"].as_array().expect("templates") {
        let template = case["template"].as_str().expect("template");
        let resolved = resolve_config_value(template, &lookup).expect("template value");
        assert_eq!(opt(resolved), case["resolved"], "template {template}");
    }
}

#[test]
fn config_paths_match_typescript() {
    let fx = fixture();
    assert_eq!(fx["envAgentDir"], ENV_AGENT_DIR);
    let root = root();
    let home = root.path.join("fakehome");
    for case in fx["paths"].as_array().expect("paths") {
        let env = case["env"].as_str().map(str::to_string);
        let dir = agent_dir(|name| if name == ENV_AGENT_DIR { env.clone() } else { None }, &home);
        let p = AgentPaths::new(&dir);
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        let got = json!({
            "env": case["env"], "agentDir": s(dir.clone()), "auth": s(p.auth()), "settings": s(p.settings()),
            "models": s(p.models()), "themes": s(p.custom_themes_dir()), "tools": s(p.tools_dir()), "bin": s(p.bin_dir()),
            "prompts": s(p.prompts_dir()), "sessions": s(p.sessions_dir()), "debugLog": s(p.debug_log()),
        });
        assert_eq!(root.rel(got), *case, "agent dir for {:?}", case["env"]);
    }
}

#[test]
fn config_trust_store_matches_typescript() {
    let fx = fixture();
    let trust = &fx["trust"];
    let root = root();
    let base = root.path.join("trust");
    let work = base.join("work");
    for dir in [
        "work/a/b",
        "work/pi/.pi/extensions",
        "work/agents/.agents/skills",
        "work/plain",
        "home/.agents/skills",
        "work/link-target",
        "work/settings/.pi",
    ] {
        fs::create_dir_all(base.join(dir)).expect("mkdir");
    }
    fs::write(base.join("work/pi2.txt"), "").expect("write");
    fs::write(base.join("work/settings/.pi/settings.json"), "{}").expect("write");
    std::os::unix::fs::symlink(work.join("link-target"), work.join("link")).expect("symlink");
    let home = base.join("home");
    let store = ProjectTrustStore::new(&base.join("agent"));
    let trust_path = base.join("agent").join("trust.json");
    for (n, step) in trust["steps"].as_array().expect("steps").iter().enumerate() {
        let at = |key: &str| base.join(step[key].as_str().expect("path"));
        let result = match step["op"].as_str().expect("op") {
            "write" => {
                fs::write(&trust_path, step["text"].as_str().expect("text")).expect("write");
                Ok(Value::Null)
            }
            "get" => store.get(&at("path")).map(opt),
            "getEntry" => store.get_entry(&at("path")).map(|e| {
                e.map_or(
                    Value::Null,
                    |(p, d)| json!({ "path": p.to_string_lossy(), "decision": d }),
                )
            }),
            "set" => store.set(&at("path"), step["decision"].as_bool()).map(|()| Value::Null),
            "setMany" => {
                let updates: Vec<TrustUpdate> = step["updates"]
                    .as_array()
                    .expect("updates")
                    .iter()
                    .map(|u| TrustUpdate {
                        path: base.join(u[0].as_str().expect("path")),
                        decision: u[1].as_bool(),
                    })
                    .collect();
                store.set_many(&updates).map(|()| Value::Null)
            }
            other => panic!("unmapped trust op {other}"),
        };
        let what = format!("trust step {n} {}", step["op"]);
        match (result, step["result"].get("error").and_then(Value::as_str)) {
            (Err(e), Some(expected)) if expected.contains("is not valid JSON") => {
                let e = root.rel(Value::String(e));
                let prefix = expected.split(": ").next().expect("prefix");
                assert!(
                    e.as_str().expect("str").starts_with(&format!("{prefix}: ")),
                    "{what}: {e}"
                );
            }
            (Err(e), Some(_)) => assert_eq!(root.rel(json!({ "error": e })), step["result"], "{what}"),
            (result, _) => assert_eq!(root.rel(result.expect("ok")), step["result"], "{what}"),
        }
        assert_eq!(root.read(&trust_path), step["file"], "file after {what}");
    }
    for case in trust["resources"].as_array().expect("resources") {
        let path = base.join(case["path"].as_str().expect("path"));
        assert_eq!(
            has_trust_requiring_project_resources(&path, &home),
            case["requires"],
            "resources at {path:?}"
        );
    }
    let options_json = |cwd: &Path, session: bool| -> Value {
        let options = trust_options(cwd, session);
        Value::Array(
            options
                .into_iter()
                .map(|o| {
                    let updates: Vec<Value> = o
                        .updates
                        .iter()
                        .map(|u| json!({ "path": u.path.to_string_lossy(), "decision": opt(u.decision) }))
                        .collect();
                    let mut v = json!({ "label": o.label, "trusted": o.trusted, "updates": updates });
                    if let Some(saved) = o.saved_path {
                        v["savedPath"] = saved.to_string_lossy().into();
                    }
                    v
                })
                .collect(),
        )
    };
    let expected = trust["options"].as_array().expect("options");
    assert_eq!(root.rel(options_json(&work.join("a"), true)), expected[0]);
    assert_eq!(root.rel(options_json(&work.join("a"), false)), expected[1]);
    assert_eq!(options_json(Path::new("/"), false), trust["rootOptions"]);
}

#[test]
fn config_models_store_matches_typescript() {
    let fx = fixture();
    let root = root();
    let path = root.path.join("models").join("models-store.json");
    let store = ModelsStore::new(&path);
    for (n, step) in fx["modelsStore"].as_array().expect("steps").iter().enumerate() {
        let provider = step["provider"].as_str().unwrap_or_default();
        let result = match step["op"].as_str().expect("op") {
            "external" => {
                fs::write(&path, step["text"].as_str().expect("text")).expect("write");
                Ok(Value::Null)
            }
            "read" => store.read(provider).map(|e| e.unwrap_or(Value::Null)),
            "write" => store.write(provider, step["entry"].clone()).map(|()| Value::Null),
            "delete" => store.delete(provider).map(|()| Value::Null),
            other => panic!("unmapped models-store op {other}"),
        };
        let result = result.unwrap_or_else(|_| json!({ "error": true }));
        assert_eq!(result, step["result"], "models-store step {n} {}", step["op"]);
        assert_eq!(root.read(&path), step["file"], "models-store file after step {n}");
    }
}
