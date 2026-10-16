//! The skills, prompt templates and system prompt files a session loads, replayed from
//! `fixtures/session_resources.json` (recorded from TypeScript's `DefaultResourceLoader.reload` by
//! `fixtures/generate-session-resources.mjs`): the settings' local `skills`/`prompts` entries with their patterns,
//! the auto-discovered directories (`~/.agents/skills` and the ancestor `.agents/skills` included), the bundled
//! skills, SYSTEM.md / APPEND_SYSTEM.md, and a project that is not trusted contributing none of its resources. The
//! lists, their order, every `SourceInfo` (also in the command list), the prompt texts and the system prompt built from
//! them must match. Every case runs in a private temp root that is also HOME.

use resources::system_prompt::{BuildOptions, build_system_prompt};
use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::session_flags::SessionFlags;
use session_core::session_resources::{self, SessionResources};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/session_resources.json")).unwrap()
}

fn case(name: &str) -> Value {
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no case {name}"))
}

struct Root {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Root {
    fn text(&self) -> String {
        self.root.display().to_string()
    }
    fn sub(&self, value: &Value) -> Value {
        serde_json::from_str(&value.to_string().replace(&self.text(), "<TMP>")).unwrap()
    }
    fn cwd(&self) -> String {
        self.root.join("work").display().to_string()
    }
    fn agent(&self) -> String {
        self.root.join("agent").display().to_string()
    }
    fn bundled(&self) -> String {
        self.root.join("bundled").display().to_string()
    }
}

/// The case's files and settings under a fresh temp root.
fn materialize(case: &Value) -> Root {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for (rel, text) in case["files"].as_object().unwrap() {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text.as_str().unwrap()).unwrap();
    }
    std::fs::write(root.join("agent/settings.json"), case["settings"].to_string()).unwrap();
    if !case["project"].is_null() {
        std::fs::write(root.join("work/.pi/settings.json"), case["project"].to_string()).unwrap();
    }
    Root { _tmp: tmp, root }
}

fn run_env(root: &Path) -> RunEnv {
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), root.join("home").display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            root.join("agent").display().to_string(),
        ),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("PI_OFFLINE".into(), "1".into()),
    ];
    RunEnv {
        cwd: root.join("work"),
        home: root.join("home"),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: root.join("docs"),
    }
}

fn resolve(root: &Root, case: &Value) -> SessionResources {
    let env = run_env(&root.root);
    let settings =
        config::settings::SettingsManager::create(&env.cwd, &root.root.join("agent"), case["trusted"] == true);
    let flags = SessionFlags {
        no_skills: case["noSkills"] == true,
        ..SessionFlags::default()
    };
    let resolved = session_resources::resolve(&env, settings, &flags);
    assert!(resolved.diagnostics.is_empty(), "{:?}", resolved.diagnostics);
    resolved
}

#[test]
fn session_core_resources_match_typescript_resource_loader() {
    let mut diffs = Vec::new();
    for case in fixture()["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let root = materialize(case);
        let resources = resolve(&root, case);
        let no_skills = case["noSkills"] == true;
        let (cwd, agent, bundled) = (root.cwd(), root.agent(), root.bundled());
        let skills = session_resources::skills(&cwd, &agent, &resources, no_skills, Some(&bundled));
        let skill_rows: Vec<Value> = skills
            .iter()
            .map(|s| {
                root.sub(&json!({
                    "name": s.name, "filePath": s.file_path,
                    "sourceInfo": session_resources::skill_source_info(&resources, s),
                }))
            })
            .collect();
        let templates = session_resources::prompts(&cwd, &agent, &resources);
        let prompt_rows: Vec<Value> = templates
            .iter()
            .map(|t| {
                root.sub(&json!({
                    "name": t.name, "filePath": t.file_path,
                    "sourceInfo": session_resources::prompt_source_info(&resources, t, &cwd, &agent),
                }))
            })
            .collect();
        let mut check = |what: &str, rust: Value, node: &Value| {
            if rust != *node {
                diffs.push(format!("{name} {what}:\n  rust {rust}\n  node {node}"));
            }
        };
        check("skills", Value::from(skill_rows.clone()), &case["skills"]);
        check("prompts", Value::from(prompt_rows.clone()), &case["prompts"]);
        check("systemPrompt", json!(resources.system_prompt), &case["systemPrompt"]);
        check(
            "appendSystemPrompt",
            json!(resources.append_system_prompt),
            &case["appendSystemPrompt"],
        );
        // The command list (`get_commands`) carries the same `SourceInfo`s (`resourceSourceInfo`).
        let commands = session_core::commands::list_commands_with(&cwd, &agent, Some(&bundled), &[], &resources);
        let listed = |prefix: &str, rows: &[Value]| -> (Value, Value) {
            let from_commands: Vec<Value> = commands
                .iter()
                .filter(|c| c["source"] == prefix)
                .map(|c| root.sub(&c["sourceInfo"]))
                .collect();
            let from_rows: Vec<Value> = rows.iter().map(|r| r["sourceInfo"].clone()).collect();
            (Value::from(from_commands), Value::from(from_rows))
        };
        let skills_expected: Vec<Value> = if no_skills {
            // The command list holds the skills a `--no-skills` session filters out (session_flags::filter_commands).
            let all = session_resources::skills(&cwd, &agent, &resources, false, Some(&bundled));
            all.iter()
                .map(|s| root.sub(&json!({"sourceInfo": session_resources::skill_source_info(&resources, s)})))
                .collect()
        } else {
            skill_rows.clone()
        };
        let (got, want) = listed("skill", &skills_expected);
        check("command skills", got, &want);
        let (got, want) = listed("prompt", &prompt_rows);
        // `/review` is listed first, without a source.
        check(
            "command prompts",
            got,
            &json!(
                [Value::Null]
                    .into_iter()
                    .chain(want.as_array().unwrap().clone())
                    .collect::<Vec<_>>()
            ),
        );
        let options = BuildOptions {
            selected_tools: vec!["rlm".into()],
            tool_snippets: vec![("rlm".into(), "Run Python in the persistent REPL.".into())],
            custom_prompt: resources.system_prompt.clone(),
            append_system_prompt: resources.append_system_prompt.join("\n\n"),
            skills,
            include_harness_docs: false,
            ..BuildOptions::new(&cwd)
        };
        let prompt = build_system_prompt(&options, None).unwrap();
        check("prompt", json!(prompt.replace(&root.text(), "<TMP>")), &case["prompt"]);
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

/// The session's own system prompt (the lane's, with the runtime guide) opens with the discovered SYSTEM.md and
/// carries APPEND_SYSTEM.md as its addendum; in a project that is not trusted, the profile's files and none of the
/// project's skills.
#[test]
fn session_core_resources_shape_the_session_system_prompt() {
    for (name, absent) in [
        ("defaults-trusted-project", "Profile system prompt."),
        ("untrusted-project-loads-no-project-resources", "Project system prompt."),
    ] {
        let case = case(name);
        let root = materialize(&case);
        let resources = resolve(&root, &case);
        let env = run_env(&root.root);
        let (prompt, warnings) = session_core::lane::system_prompt_with_resources(
            &env,
            &root.cwd(),
            None,
            None,
            &Default::default(),
            &Default::default(),
            &resources,
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let custom = case["systemPrompt"].as_str().unwrap();
        assert!(prompt.starts_with(custom), "{name}: {prompt}");
        assert!(!prompt.contains(absent), "{name}");
        let addendum = format!(
            "<addendum>\n{}\n</addendum>",
            case["appendSystemPrompt"][0].as_str().unwrap()
        );
        assert!(prompt.contains(&addendum), "{name}: {prompt}");
        let ts = case["prompt"].as_str().unwrap();
        for skill in case["skills"].as_array().unwrap() {
            let file = skill["filePath"].as_str().unwrap().replace("<TMP>", &root.text());
            // Bundled skills come from the package's own directory in the session; the fixture's are not there.
            if file.starts_with(&root.bundled()) {
                continue;
            }
            assert!(prompt.contains(&file), "{name}: {file} missing");
            assert!(ts.contains(skill["filePath"].as_str().unwrap()));
        }
        let project_skill = root.root.join("work/.pi/skills/proj-skill/SKILL.md");
        assert_eq!(
            prompt.contains(&project_skill.display().to_string()),
            case["trusted"] == true,
            "{name}"
        );
    }
}

/// A runtime's resources follow the worker's project trust (`workerProjectTrusted`): a project the user distrusted
/// (`trust.json`) gives its skills, prompts and SYSTEM.md to no one; with no decision it is trusted.
#[test]
fn session_core_resources_follow_the_stored_trust_decision() {
    let case = case("defaults-trusted-project");
    let root = materialize(&case);
    let env = run_env(&root.root);
    let trusted = session_resources::resolve_default(&env);
    assert_eq!(trusted.system_prompt.as_deref(), Some("Project system prompt.\n"));
    assert!(
        trusted
            .skill_paths
            .iter()
            .any(|p| p.ends_with("work/.pi/skills/proj-skill/SKILL.md"))
    );
    config::trust::ProjectTrustStore::new(&root.root.join("agent"))
        .set(&root.root.join("work"), Some(false))
        .unwrap();
    let distrusted = session_resources::resolve_default(&env);
    assert_eq!(distrusted.system_prompt.as_deref(), Some("Profile system prompt.\n"));
    assert_eq!(distrusted.append_system_prompt, vec!["Profile addendum.\n".to_string()]);
    let project = root.root.join("work").display().to_string();
    let ancestor = root.root.join(".agents").display().to_string();
    assert!(
        distrusted
            .skill_paths
            .iter()
            .chain(&distrusted.prompt_paths)
            .all(|p| !p.starts_with(&project) && !p.starts_with(&ancestor)),
        "{:?} {:?}",
        distrusted.skill_paths,
        distrusted.prompt_paths
    );
    assert!(
        distrusted
            .skill_paths
            .iter()
            .any(|p| p.ends_with("home/.agents/skills/home-agent/SKILL.md"))
    );
}

/// A settings package that cannot be installed (the configured npm command fails) is reported, and the local and
/// auto-discovered resources still load (TypeScript's reload throws; the Rust worker keeps the session usable).
#[test]
fn session_core_resources_keep_local_ones_when_a_package_fails() {
    use std::os::unix::fs::PermissionsExt;
    let case = case("local-entries-with-patterns");
    let root = materialize(&case);
    let npm = root.root.join("bin/fake-npm");
    std::fs::create_dir_all(npm.parent().unwrap()).unwrap();
    std::fs::write(&npm, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut settings = case["settings"].clone();
    settings["packages"] = json!(["npm:fixture-missing-package@1.0.0"]);
    settings["npmCommand"] = json!([npm.display().to_string()]);
    std::fs::write(root.root.join("agent/settings.json"), settings.to_string()).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), root.root.join("home").display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), root.agent()),
        ("PATH".into(), root.root.join("bin").display().to_string()),
    ];
    let env = RunEnv {
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        ..run_env(&root.root)
    };
    let manager = config::settings::SettingsManager::create(&env.cwd, &root.root.join("agent"), true);
    let resources = session_resources::resolve(&env, manager, &SessionFlags::default());
    assert_eq!(resources.diagnostics.len(), 1, "{:?}", resources.diagnostics);
    assert!(
        resources.diagnostics[0].contains("failed with code 1"),
        "{:?}",
        resources.diagnostics
    );
    let skills = session_resources::skills(&root.cwd(), &root.agent(), &resources, false, Some(&root.bundled()));
    let want: Vec<&str> = case["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    let got: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(got, want);
}

/// A worker given a skill directory in `skillPaths` (`-e` resources are directories too) gives every skill in it the
/// directory's `cli` metadata (`findSourceInfoForPath`'s parent-directory match).
#[test]
fn session_core_resources_skills_under_a_cli_directory_are_cli() {
    let case = case("untrusted-project-loads-no-project-resources");
    let root = materialize(&case);
    let env = run_env(&root.root);
    let dir = root.root.join("extra/skills").display().to_string();
    let flags = SessionFlags {
        skill_paths: vec![dir.clone()],
        ..SessionFlags::default()
    };
    let settings = config::settings::SettingsManager::create(&env.cwd, &root.root.join("agent"), false);
    let resources = session_resources::resolve(&env, settings, &flags);
    let skills = session_resources::skills(&root.cwd(), &root.agent(), &resources, false, None);
    let extra: Vec<Value> = skills
        .iter()
        .filter(|s| s.file_path.starts_with(&dir))
        .map(|s| root.sub(&session_resources::skill_source_info(&resources, s)))
        .collect();
    assert_eq!(
        extra,
        ["extra-one", "extra-two", "skip-me"]
            .iter()
            .map(|name| json!({
                "path": format!("<TMP>/extra/skills/{name}/SKILL.md"),
                "source": "cli", "scope": "temporary", "origin": "top-level",
            }))
            .collect::<Vec<_>>()
    );
}
