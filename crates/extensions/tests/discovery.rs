//! Extension process configuration: manifests in the TypeScript extension locations, in package-manager order.

use extensions::{discover, discover_with_settings, parse_manifest};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::time::Duration;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn ext(dir: &Path, name: &str) {
    write(
        &dir.join("extension.json"),
        &json!({ "name": name, "command": "python3", "args": ["main.py"] }).to_string(),
    );
}

fn settings(entries: &[&str]) -> Map<String, Value> {
    json!({ "extensions": entries }).as_object().unwrap().clone()
}

#[test]
fn extensions_discovery_follows_package_manager_order_and_trust() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let agent = root.path().join("agent");
    let cwd = root.path().join("project");
    ext(&cwd.join(".pi/extensions/proj-auto"), "proj-auto");
    ext(&agent.join("extensions/user-auto"), "user-auto");
    ext(&cwd.join(".pi/local/proj-entry"), "proj-entry");
    ext(&agent.join("mine/user-entry"), "user-entry");
    ext(&home.join("tools/tilde"), "tilde");
    let global = settings(&[
        "mine/user-entry",
        "~/tools/tilde/extension.json",
        "extensions/user-auto",
    ]);
    let project = settings(&["local/proj-entry"]);

    let trusted = discover(&cwd, &agent, &home, &global, &project, true);
    let names: Vec<&str> = trusted.manifests.iter().map(|m| m.name.as_str()).collect();
    // Project entries, global entries, project auto dir, user auto dir; user-auto keeps its first (entry) position.
    assert_eq!(names, ["proj-entry", "user-entry", "tilde", "user-auto", "proj-auto"]);
    assert!(trusted.diagnostics.is_empty(), "{:?}", trusted.diagnostics);
    assert_eq!(trusted.manifests[0].dir, cwd.join(".pi/local/proj-entry"));

    // Untrusted: no project settings entries, no project extensions directory.
    let untrusted = discover(&cwd, &agent, &home, &global, &project, false);
    let names: Vec<&str> = untrusted.manifests.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["user-entry", "tilde", "user-auto"]);
}

#[test]
fn extensions_discovery_reports_javascript_extensions_and_bad_entries() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let cwd = root.path().join("project");
    write(&agent.join("extensions/old.ts"), "export default () => {}");
    write(
        &agent.join("extensions/pkg/package.json"),
        r#"{"pi":{"extensions":["./index.ts"]}}"#,
    );
    write(&agent.join("extensions/notes.txt"), "not an extension");
    std::fs::create_dir_all(agent.join("extensions/empty")).unwrap();
    write(&agent.join("extensions/broken/extension.json"), r#"{"name":"broken"}"#);
    let global = settings(&["./gone", "!excluded/**", "../legacy.js"]);
    write(&root.path().join("legacy.js"), "");
    let found = discover(&cwd, &agent, root.path(), &global, &Map::new(), true);
    assert!(found.manifests.is_empty());
    let messages: Vec<String> = found.diagnostics.iter().map(|d| d.message.clone()).collect();
    assert_eq!(messages.len(), 6, "{messages:?}");
    assert_eq!(
        messages[0],
        format!("Extension path does not exist: {}", agent.join("gone").display())
    );
    assert_eq!(
        messages[1],
        "Extension pattern \"!excluded/**\" is not supported for extension processes; list manifest paths"
    );
    assert!(messages[2].starts_with("JavaScript/TypeScript extensions are no longer loaded"));
    assert!(messages[2].contains("legacy.js"));
    // Auto directory, sorted: broken/, old.ts, pkg/.
    assert_eq!(
        messages[3],
        "Invalid extension manifest: \"command\" must be a non-empty string"
    );
    assert!(messages[4].contains("old.ts"));
    assert!(messages[5].contains("extensions/pkg"));
}

#[test]
fn extensions_discovery_reads_settings_files_through_config() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let cwd = root.path().join("project");
    ext(&agent.join("x"), "from-global");
    ext(&cwd.join(".pi/y"), "from-project");
    write(&agent.join("settings.json"), r#"{"extensions":["x"]}"#);
    write(&cwd.join(".pi/settings.json"), r#"{"extensions":["y"]}"#);
    let manager = config::settings::SettingsManager::create(&cwd, &agent, true);
    let found = discover_with_settings(&cwd, &agent, root.path(), &manager);
    let names: Vec<&str> = found.manifests.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["from-project", "from-global"]);
}

#[test]
fn extensions_manifest_parsing() {
    let path = Path::new("/exts/hello/extension.json");
    let m = parse_manifest(
        path,
        r#"{"command":"./bin/run","args":["a"],"env":{"K":"v"},"eventTimeoutMs":250,"startupTimeoutMs":900,"maxRestarts":0}"#,
    )
    .unwrap();
    assert_eq!(m.name, "hello");
    assert_eq!(m.command, "/exts/hello/./bin/run");
    assert_eq!(m.args, ["a"]);
    assert_eq!(m.env, [("K".to_string(), "v".to_string())]);
    assert_eq!(m.event_timeout, Duration::from_millis(250));
    assert_eq!(m.startup_timeout, Duration::from_millis(900));
    assert_eq!(m.max_restarts, 0);
    let defaults = parse_manifest(path, r#"{"name":"n","command":"python3"}"#).unwrap();
    assert_eq!(defaults.command, "python3");
    assert_eq!(defaults.event_timeout, Duration::from_secs(30));
    assert_eq!(defaults.startup_timeout, Duration::from_secs(10));
    assert_eq!(defaults.max_restarts, 3);
    for (text, error) in [
        ("[]", "manifest must be a JSON object"),
        (r#"{"command":""}"#, "\"command\" must be a non-empty string"),
        (r#"{"command":"x","args":"a"}"#, "\"args\" must be an array of strings"),
        (r#"{"command":"x","env":{"K":1}}"#, "\"env\" values must be strings"),
        (
            r#"{"command":"x","eventTimeoutMs":-1}"#,
            "\"eventTimeoutMs\" must be a non-negative integer of milliseconds",
        ),
        (r#"{"command":"x","name":""}"#, "\"name\" must be a non-empty string"),
    ] {
        assert_eq!(parse_manifest(path, text).unwrap_err(), error, "{text}");
    }
}
