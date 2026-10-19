//! Themes registered by the settings and `<agentDir>/themes` (client-tui.ts
//! `setRegisteredThemes(resourceLoader.getThemes().themes)`) against `fixtures/local_themes.json`, recorded from the
//! TypeScript resource loader by `fixtures/local_themes.mjs` over the same profile: which themes register, in which order, the collision and the
//! file that does not load, and what `/settings` offers (`getAvailableThemesWithPaths`); then the TUI's theme
//! controller applies one of them by name.

use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tui::theme::ColorMode;
use tui::theme::registry::ThemeRegistry;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/local_themes.json")).unwrap()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// The generator's profile under `root`.
fn profile(root: &Path) -> cli::Env {
    let dark: Value = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../packages/coding-agent/src/modes/interactive/theme/dark.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let theme = |name: &str| {
        let mut json = dark.clone();
        json["name"] = json!(name);
        serde_json::to_string_pretty(&json).unwrap()
    };
    let agent = root.join("agent");
    write(&root.join("extra/ocean/ocean.json"), &theme("ocean"));
    write(&root.join("extra/ocean/abyss.json"), &theme("abyss"));
    write(&root.join("extra/ocean/README.txt"), "not a theme");
    write(&root.join("extra/second/ocean.json"), &theme("ocean"));
    write(&root.join("extra/second/zest.json"), &theme("zest"));
    write(&root.join("extra/solo.json"), &theme("solo"));
    write(&agent.join("themes/custom.json"), &theme("custom"));
    write(&root.join("extra/broken.json"), "{ not json");
    let r = |p: &str| root.join(p).display().to_string();
    write(
        &agent.join("settings.json"),
        &json!({
            "themes": [
                r("extra/solo.json"), r("extra/ocean"), r("extra/second"), r("extra/broken.json"), r("extra/missing.json"),
            ],
        })
        .to_string(),
    );
    std::fs::create_dir_all(root.join("cwd")).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), root.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("PI_OFFLINE".into(), "1".into()),
    ];
    cli::Env {
        cwd: root.join("cwd"),
        home: root.to_path_buf(),
        var: Arc::new(move |k| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
        docs: std::path::PathBuf::new(),
    }
}

fn clean(path: &str, root: &Path) -> String {
    path.replace(&root.display().to_string(), "<root>")
}

#[test]
fn cli_local_themes_register_like_typescript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let env = profile(&root);
    let loaded = cli::themes::load(&env, ColorMode::Truecolor);
    let registered: Vec<Value> = loaded
        .themes
        .iter()
        .map(|t| json!({"name": t.name, "sourcePath": t.source_path.as_deref().map(|p| clean(p, &root))}))
        .collect();
    assert_eq!(json!(registered), fixture()["registered"]);
    // The problems as the loader reports them (the parse error's wording is the JSON parser's own).
    let diagnostics: Vec<(String, String)> = loaded
        .diagnostics
        .iter()
        .map(|(kind, path)| (kind.clone(), clean(path, &root)))
        .collect();
    let expected: Vec<(String, String)> = fixture()["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["type"].as_str().unwrap().to_string(),
                d["path"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(diagnostics, expected);
}

#[test]
fn cli_local_themes_are_offered_and_applied_by_the_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let env = profile(&root);
    let mut registry = ThemeRegistry::new(
        Path::new("<builtin>").to_path_buf(),
        env.agent_dir().join("themes"),
        ColorMode::Truecolor,
    );
    registry
        .set_registered_themes(cli::themes::load(&env, registry.mode).themes)
        .unwrap();
    let available: Vec<Value> = registry
        .available_themes_with_paths()
        .iter()
        .map(|t| json!({"name": t.name, "path": t.path.as_deref().map(|p| clean(p, &root))}))
        .collect();
    assert_eq!(json!(available), fixture()["available"]);
    // `/settings` theme: choosing a settings theme applies it.
    let mut controller = interactive::theme_setup::ThemeController::new(registry, Some("dark".into()), None);
    assert!(controller.available().contains(&"zest".to_string()));
    let applied = controller.set_setting("zest");
    assert_eq!((applied.theme.name.as_deref(), applied.error), (Some("zest"), None));
    assert_eq!(
        applied.theme.source_path.as_deref().map(|p| clean(p, &root)),
        Some("<root>/extra/second/zest.json".to_string())
    );
}

mod tui_harness;

/// At startup the TUI registers the settings themes, so a `theme` setting naming one draws with it (every color of
/// `zest` is #123456 here); a name nothing registers falls back to dark.
#[test]
fn cli_interactive_theme_setting_names_a_local_theme() {
    let mock = tui_harness::Mock::start(Vec::new(), None);
    for (setting, painted) in [("zest", true), ("nosuch", false)] {
        let tmp = tui_harness::TempDir::new();
        profile(&tmp.0);
        let zest = tmp.0.join("extra/second/zest.json");
        let text = std::fs::read_to_string(&zest).unwrap();
        let recolored = fancy_hex(&text);
        std::fs::write(&zest, recolored).unwrap();
        let settings = tmp.0.join("agent/settings.json");
        let mut json: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        json["theme"] = json!(setting);
        std::fs::write(&settings, json.to_string()).unwrap();
        let tui = tui_harness::Tui::start_at(
            &tmp.0,
            mock.port,
            100,
            30,
            &["--model", "anthropic/claude-sonnet-4-5"],
            &[("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")],
        );
        tui.wait("Session:");
        let colored = tui.tmux(&["capture-pane", "-p", "-e", "-t", "t"]);
        assert_eq!(colored.contains("18;52;86"), painted, "{setting}: {colored}");
        drop(tui);
    }
}

/// Every `#rrggbb` color in a theme file as #123456.
fn fancy_hex(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && i + 7 <= bytes.len() && bytes[i + 1..i + 7].iter().all(u8::is_ascii_hexdigit) {
            out.push_str("#123456");
            i += 7;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}
