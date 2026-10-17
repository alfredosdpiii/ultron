//! `.gitignore`, `.ignore` and `.fdignore` inside skill directories (core/skills.ts `addIgnoreRules`), replayed from
//! fixtures/skill-ignores.json (generate-skill-ignores.mjs runs the TypeScript `loadSkills` over the same tree):
//! the default locations and explicit paths (`--skill`, `-e` directories) skip what the rules ignore.

use resources::skills::{LoadSkillsOptions, load_skills};
use serde_json::{Value, json};
use std::path::Path;

#[test]
fn resources_skill_directories_honour_ignore_files_like_typescript() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/skill-ignores.json")).expect("skill-ignores.json parses");
    for case in fixture["cases"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().display().to_string();
        for entry in fixture["tree"].as_array().unwrap() {
            let full = Path::new(&root).join(entry[0].as_str().unwrap());
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, entry[1].as_str().unwrap()).unwrap();
        }
        let paths: Vec<String> = case["skillPaths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().replace("$ROOT", &root))
            .collect();
        let result = load_skills(&LoadSkillsOptions {
            cwd: &format!("{root}/proj"),
            agent_dir: &format!("{root}/agent"),
            skill_paths: &paths,
            include_defaults: case["includeDefaults"].as_bool().unwrap(),
        });
        let scrub = |text: &str| text.replace(&root, "$ROOT");
        let skills: Vec<Value> = result
            .skills
            .iter()
            .map(|s| json!({"name": s.name, "filePath": scrub(&s.file_path), "baseDir": scrub(&s.base_dir)}))
            .collect();
        assert_eq!(Value::Array(skills), case["skills"], "{:?}", case["skillPaths"]);
        let diagnostics: Vec<Value> = result
            .diagnostics
            .iter()
            .map(|d| json!({"type": d.kind, "message": scrub(&d.message), "path": d.path.as_deref().map(scrub)}))
            .collect();
        assert_eq!(
            Value::Array(diagnostics),
            case["diagnostics"],
            "{:?}",
            case["skillPaths"]
        );
    }
}
