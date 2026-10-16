//! The skills and prompts a session loads from `-e` sources and settings `packages` beside its own, replayed from
//! `fixtures/session_resources.json` (recorded from TypeScript's `DefaultResourceLoader.reload` by
//! `fixtures/session_resources.mjs`): the CLI resolves the `-e` sources (`session_flags`), the session resolves the
//! settings packages (`session_resources::resolve`), and the merged lists, their order and the commands'
//! `sourceInfo` must match.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/session_resources.json")).unwrap()
}

fn env(root: &Path) -> cli::Env {
    let vars: HashMap<String, String> = [
        ("HOME", root.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR", root.join("agent").display().to_string()),
        ("ULTRON_SERVER_DIR", root.join("server").display().to_string()),
        ("PI_OFFLINE", "1".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    cli::Env {
        cwd: root.join("work"),
        home: root.to_path_buf(),
        var: Arc::new(move |k| vars.get(k).cloned()),
        docs: root.join("docs"),
    }
}

struct Replayed {
    skills: Vec<Value>,
    prompts: Vec<Value>,
    flags: session_core::session_flags::SessionFlags,
}

fn replay(case: &Value) -> Replayed {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for (rel, text) in case["files"].as_object().unwrap() {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text.as_str().unwrap()).unwrap();
    }
    std::fs::create_dir_all(root.join("work")).unwrap();
    std::fs::write(root.join("agent/settings.json"), case["settings"].to_string()).unwrap();
    if !case["project"].is_null() {
        std::fs::create_dir_all(root.join("work/.pi")).unwrap();
        std::fs::write(root.join("work/.pi/settings.json"), case["project"].to_string()).unwrap();
    }
    let run_env = env(&root);
    let mut argv: Vec<String> = Vec::new();
    for source in case["extensions"].as_array().unwrap() {
        argv.extend(["-e".to_string(), source.as_str().unwrap().to_string()]);
    }
    if case["noSkills"] == true {
        argv.push("--no-skills".into());
    }
    let parsed = cli::args::parse_args(&argv);
    let flags = cli::print_mode::session_flags(&parsed, &run_env).unwrap();
    let (cwd, agent_dir) = (root.join("work"), root.join("agent"));
    let settings = config::settings::SettingsManager::create(&cwd, &agent_dir, case["trusted"] == true);
    let resources = session_core::session_resources::resolve(&run_env, settings, &flags);
    assert!(resources.diagnostics.is_empty(), "{:?}", resources.diagnostics);
    let (cwd, agent_dir) = (cwd.display().to_string(), agent_dir.display().to_string());
    let tmp_text = root.display().to_string();
    let sub = |s: &str| s.replace(&tmp_text, "<TMP>");
    let commands = session_core::commands::list_commands_with(&cwd, &agent_dir, "/nonexistent", &[], &resources);
    let info = |name: &str| -> Value {
        let found = commands
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or(Value::Null);
        serde_json::from_str(&sub(&found["sourceInfo"].to_string())).unwrap()
    };
    let skills = session_core::session_resources::skills(&cwd, &agent_dir, &resources, flags.no_skills)
        .into_iter()
        .map(|s| json!({"name": s.name, "filePath": sub(&s.file_path), "sourceInfo": info(&format!("skill:{}", s.name))}))
        .collect();
    let prompts = session_core::session_resources::prompts(&cwd, &agent_dir, &resources)
        .into_iter()
        .map(|p| json!({"name": p.name, "filePath": sub(&p.file_path)}))
        .collect();
    let flags = session_core::session_flags::SessionFlags {
        skill_paths: flags.skill_paths.iter().map(|p| sub(p)).collect(),
        prompt_paths: flags.prompt_paths.iter().map(|p| sub(p)).collect(),
        ..flags
    };
    Replayed { skills, prompts, flags }
}

#[test]
fn cli_session_resources_match_typescript_resource_loader() {
    let fx = fixture();
    let mut diffs = Vec::new();
    for case in fx["cases"].as_array().unwrap() {
        let replayed = replay(case);
        let name = case["name"].as_str().unwrap();
        let expected_skills: Vec<Value> = case["skills"].as_array().unwrap().clone();
        let skills = replayed.skills.clone();
        if skills != expected_skills {
            diffs.push(format!(
                "{name} skills:\n  rust {}\n  node {}",
                Value::Array(skills),
                Value::Array(expected_skills)
            ));
        }
        if replayed.prompts != *case["prompts"].as_array().unwrap() {
            diffs.push(format!(
                "{name} prompts:\n  rust {}\n  node {}",
                Value::Array(replayed.prompts),
                case["prompts"]
            ));
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

/// The `-e` source's skills and prompts reach the worker through its options (`skillPaths`, `promptPaths`).
#[test]
fn cli_session_resources_cli_paths_travel_in_the_worker_options() {
    let fx = fixture();
    let case = fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "cli-source-only")
        .unwrap();
    let replayed = replay(case);
    assert_eq!(
        replayed.flags.skill_paths,
        vec![
            "<TMP>/pkgs/beta/skills/beta-skill/SKILL.md",
            "<TMP>/pkgs/beta/skills/mine/SKILL.md"
        ]
    );
    assert_eq!(
        replayed.flags.prompt_paths,
        vec!["<TMP>/pkgs/beta/prompts/beta-prompt.md"]
    );
    let mut options = json!({});
    replayed.flags.write_worker_options(&mut options);
    assert_eq!(options["skillPaths"], json!(replayed.flags.skill_paths));
    assert_eq!(options["promptPaths"], json!(replayed.flags.prompt_paths));
    assert_eq!(
        session_core::session_flags::SessionFlags::from_worker_options(&options),
        replayed.flags
    );
}
