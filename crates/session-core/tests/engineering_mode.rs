//! Engineering mode (the `engineering` setting, `/engineering`, `/mode`): the always-on skills (`alwaysSkills`) are
//! in the root system prompt, and a change of the settings applies from the session's next request without a new
//! worker (session-worker.ts `currentSystemPrompt`). Private temp profile; no model or kernel runs.

use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use std::path::Path;
use std::sync::Arc;

const BODY: &str = "Every change ships with the test that proves it.";

fn runtime(root: &Path, settings: Value) -> (Runtime, std::path::PathBuf) {
    let home = root.join("home");
    let agent = home.join(".ultron/agent");
    let skill = agent.join("skills/eng-rule");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: eng-rule\ndescription: The engineering rule.\n---\n\n{BODY}\n"),
    )
    .unwrap();
    let settings_path = agent.join("settings.json");
    std::fs::write(&settings_path, settings.to_string()).unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), Services::default()).unwrap();
    (runtime, settings_path)
}

#[test]
fn session_core_engineering_mode_puts_the_always_on_skills_in_the_prompt_and_follows_the_settings() {
    let dir = tempfile::tempdir().unwrap();
    let settings = json!({"alwaysSkills": ["eng-rule", "missing-rule"], "engineering": true});
    let (runtime, settings_path) = runtime(dir.path(), settings);
    let on = runtime.system.current();
    assert!(on.contains(BODY), "engineering mode carries the skill text:\n{on}");
    assert!(
        runtime
            .diagnostics
            .iter()
            .any(|d| d.contains("alwaysSkills: no skill named \"missing-rule\" is loaded")),
        "{:?}",
        runtime.diagnostics
    );
    // Off (as `/mode` writes it): the skill is listed like any other, without its text.
    std::fs::write(
        &settings_path,
        json!({"alwaysSkills": ["eng-rule"], "engineering": false}).to_string(),
    )
    .unwrap();
    let off = runtime.system.current();
    assert!(!off.contains(BODY), "{off}");
    assert!(off.contains("eng-rule"), "the skill is still listed:\n{off}");
    // On again: the next request's prompt has the text back; an unchanged mode renders the same prompt.
    std::fs::write(
        &settings_path,
        json!({"alwaysSkills": ["eng-rule"], "engineering": true}).to_string(),
    )
    .unwrap();
    let again = runtime.system.current();
    assert!(again.contains(BODY));
    assert_eq!(runtime.system.current(), again);
}

#[test]
fn session_core_without_engineering_mode_always_on_skills_are_listed_only() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(dir.path(), json!({"alwaysSkills": ["eng-rule"]}));
    let prompt = runtime.system.current();
    assert!(!prompt.contains(BODY), "{prompt}");
    assert!(prompt.contains("eng-rule"));
}
