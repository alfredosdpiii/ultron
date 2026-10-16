//! A session loads what its settings `packages` and its `-e` sources provide (resource-loader.ts `reload`): the
//! packages' skills join the system prompt and the command list after the profile's own, the `-e` skills come first
//! and stay with `--no-skills`, and a package's `extension.json` is discovered like a settings entry's. Private temp
//! profile with local packages only; no model, kernel or extension process runs.

use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use session_core::session_flags::SessionFlags;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn skill(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir.join(name)).unwrap();
    std::fs::write(
        dir.join(name).join("SKILL.md"),
        format!("---\nname: {name}\ndescription: The {name} skill.\n---\n\n# {name}\n"),
    )
    .unwrap();
}

struct Layout {
    _dir: tempfile::TempDir,
    root: PathBuf,
    env: RunEnv,
}

fn layout(settings: Value) -> Layout {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let agent = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent).unwrap();
    skill(&agent.join("skills"), "own-skill");
    let alpha = root.join("pkgs/alpha");
    skill(&alpha.join("skills"), "alpha-skill");
    std::fs::create_dir_all(alpha.join("prompts")).unwrap();
    std::fs::write(
        alpha.join("prompts/alpha-prompt.md"),
        "---\ndescription: Alpha's prompt\n---\nGo.\n",
    )
    .unwrap();
    std::fs::write(alpha.join("package.json"), r#"{"name":"alpha","version":"1.0.0"}"#).unwrap();
    skill(&root.join("pkgs/beta/skills"), "beta-skill");
    let settings = serde_json::to_string(&settings)
        .unwrap()
        .replace("<ROOT>", &root.display().to_string());
    std::fs::write(agent.join("settings.json"), settings).unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PI_OFFLINE".into(), "1".into()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    Layout { _dir: dir, root, env }
}

fn runtime(layout: &Layout, session: SessionFlags) -> Runtime {
    let services = Services {
        session,
        ..Services::default()
    };
    Runtime::new(&layout.env, "print", layout.root.join("sessions"), services).unwrap()
}

fn cli_flags(layout: &Layout, no_skills: bool) -> SessionFlags {
    SessionFlags {
        skill_paths: vec![
            layout
                .root
                .join("pkgs/beta/skills/beta-skill/SKILL.md")
                .display()
                .to_string(),
        ],
        no_skills,
        ..SessionFlags::default()
    }
}

#[test]
fn session_core_settings_packages_and_cli_sources_reach_the_prompt_and_commands() {
    let layout = layout(json!({"packages": ["<ROOT>/pkgs/alpha"]}));
    let runtime = runtime(&layout, cli_flags(&layout, false));
    assert!(runtime.diagnostics.is_empty(), "{:?}", runtime.diagnostics);
    let prompt = runtime.system.current();
    let at = |name: &str| {
        prompt
            .find(&format!("<name>{name}</name>"))
            .unwrap_or_else(|| panic!("{name}:\n{prompt}"))
    };
    // `-e` first, the profile's own, then the package's.
    assert!(at("beta-skill") < at("own-skill") && at("own-skill") < at("alpha-skill"));
    let model = ai::models::builtin_model("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    let commands = runtime.stack(&model, None, "packages").unwrap().commands();
    let command = |name: &str| {
        commands
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let alpha = layout.root.join("pkgs/alpha");
    assert_eq!(
        command("skill:alpha-skill")["sourceInfo"],
        json!({
            "path": alpha.join("skills/alpha-skill/SKILL.md").display().to_string(),
            "source": alpha.display().to_string(),
            "scope": "user",
            "origin": "package",
            "baseDir": alpha.display().to_string(),
        })
    );
    assert_eq!(command("skill:beta-skill")["sourceInfo"]["source"], "cli");
    assert_eq!(command("alpha-prompt")["source"], "prompt");
}

#[test]
fn session_core_no_skills_keeps_only_the_cli_sources_skills() {
    let layout = layout(json!({"packages": ["<ROOT>/pkgs/alpha"]}));
    let runtime = runtime(&layout, cli_flags(&layout, true));
    let prompt = runtime.system.current();
    assert!(prompt.contains("<name>beta-skill</name>"), "{prompt}");
    assert!(
        !prompt.contains("alpha-skill") && !prompt.contains("own-skill"),
        "{prompt}"
    );
    let model = ai::models::builtin_model("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    let skills: Vec<Value> = runtime
        .stack(&model, None, "no-skills")
        .unwrap()
        .commands()
        .into_iter()
        .filter(|c| c["source"] == "skill")
        .map(|c| c["name"].clone())
        .collect();
    assert_eq!(skills, vec![json!("skill:beta-skill")]);
}

#[test]
fn session_core_package_extension_manifests_are_discovered_after_the_profiles() {
    let layout = layout(json!({"packages": ["<ROOT>/pkgs/alpha", {"source": "<ROOT>/pkgs/off", "extensions": []}]}));
    let manifest = |dir: &Path, name: &str| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("extension.json"),
            json!({"name": name, "command": "true"}).to_string(),
        )
        .unwrap();
        dir.join("extension.json")
    };
    let packaged = manifest(&layout.root.join("pkgs/alpha"), "alpha-ext");
    manifest(&layout.root.join("pkgs/off"), "off-ext");
    let agent = layout.env.agent_dir();
    let own = manifest(&agent.join("extensions/own"), "own-ext");
    let settings = config::settings::SettingsManager::create(&layout.env.cwd, &agent, false);
    let flags = SessionFlags::default();
    let resources = session_core::session_resources::resolve(&layout.env, settings, &flags);
    let settings = config::settings::SettingsManager::create(&layout.env.cwd, &agent, false);
    let discovery = session_core::session_flags::extension_discovery_with(
        &layout.env,
        &agent,
        &settings,
        &flags,
        false,
        &resources,
    );
    let found: Vec<PathBuf> = discovery.manifests.iter().map(|m| m.path.clone()).collect();
    assert_eq!(found, vec![own, packaged.clone()], "{:?}", discovery.diagnostics);
    // `--no-extensions` leaves the packages' out with the discovered ones.
    let discovery =
        session_core::session_flags::extension_discovery_with(&layout.env, &agent, &settings, &flags, true, &resources);
    assert!(discovery.manifests.is_empty());
}

/// Extensions that did not load are kept for the TUI's `[Extension issues]` (type `error`, before the shortcut
/// warnings), as the TypeScript resource loader's `getExtensions().errors`.
#[test]
fn session_core_extension_load_errors_reach_the_extension_issues_answer() {
    let layout = layout(json!({}));
    let broken = layout.env.agent_dir().join("extensions/broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("extension.json"), r#"{"name": "broken"}"#).unwrap();
    let old = layout.env.agent_dir().join("extensions/old.ts");
    std::fs::write(&old, "export default () => {}").unwrap();
    let runtime = runtime(&layout, SessionFlags::default());
    let answer = session_core::core::extension_issues(&runtime.extension_errors, None, &[]);
    assert_eq!(
        answer["diagnostics"],
        json!([
            {
                "type": "error",
                "message": "Invalid extension manifest: \"command\" must be a non-empty string",
                "path": broken.join("extension.json").display().to_string(),
            },
            {
                "type": "error",
                "message": format!(
                    "JavaScript/TypeScript extensions are no longer loaded ({}); run it as an extension process with an extension.json (see crates/extensions/PROTOCOL.md)",
                    old.display()
                ),
                "path": old.display().to_string(),
            },
        ])
    );
    assert_eq!(answer["shortcuts"], json!([]));
}
