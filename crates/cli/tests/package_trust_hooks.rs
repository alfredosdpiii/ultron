//! `project_trust` extension hooks for the package and config commands (package-manager-cli.ts
//! `createCommandSettingsManager`), ported from test/package-command-paths.test.ts with the extension as a process
//! (crates/extensions/tests/probe.py behind an `extension.json` in the profile's extensions directory): a project
//! with project packages is untrusted until a `project_trust` handler says yes; `remember` saves the decision; an
//! undecided handler falls through; handler errors and load failures become warnings; `update` (saved trust only)
//! and `--approve`/`--no-approve` never ask; `config -l` uses the same resolution. Every run is the `ultron` binary in
//! a private temp HOME / agent dir; nothing reaches the network.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn probe() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py")
}

struct World {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    /// A project whose `.pi/settings.json` lists `npm:@project/pkg`, and (with `answer`) a user extension whose
    /// `project_trust` handler returns it (`null`: no decision).
    fn new(answer: Option<Value>) -> World {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for dir in ["agent", "home", "project/.pi", "server"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(
            root.join("project/.pi/settings.json"),
            json!({"packages": ["npm:@project/pkg"]}).to_string(),
        )
        .unwrap();
        let world = World { _temp: temp, root };
        if let Some(answer) = answer {
            world.extension("trust", &answer);
        }
        world
    }

    fn extension(&self, name: &str, answer: &Value) {
        let dir = self.root.join("agent/extensions").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = json!({
            "name": name,
            "command": python(),
            "args": ["-I", probe().to_string_lossy()],
            "env": {
                "PROBE_LOG": self.log().to_string_lossy(),
                "PROBE_CAPABILITIES": json!({"events": ["project_trust"]}).to_string(),
                "PROBE_ANSWERS": json!({"project_trust": answer}).to_string(),
            },
            "startupTimeoutMs": 120_000,
            "eventTimeoutMs": 120_000,
        });
        std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
    }

    fn log(&self) -> PathBuf {
        self.root.join("probe.log")
    }

    /// The `project_trust` events the extension saw.
    fn trust_events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|e| e["event"]["type"] == "project_trust")
            .collect()
    }

    fn run(&self, args: &[&str]) -> (String, String, i32) {
        let output = Command::new(env!("CARGO_BIN_EXE_ultron"))
            .args(args)
            .env_clear()
            .current_dir(self.root.join("project"))
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("ULTRON_CODING_AGENT_DIR", self.root.join("agent"))
            .env("ULTRON_SERVER_DIR", self.root.join("server"))
            .env("NO_COLOR", "1")
            .env("PI_OFFLINE", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.code().unwrap_or(-1),
        )
    }

    fn trust_file(&self) -> Option<Value> {
        std::fs::read_to_string(self.root.join("agent/trust.json"))
            .ok()
            .map(|t| serde_json::from_str(&t).unwrap())
    }
}

#[test]
fn cli_package_trust_skips_untrusted_project_packages_without_an_extension() {
    let world = World::new(None);
    let (out, _, code) = world.run(&["list"]);
    assert!(out.contains("No packages installed."), "{out}");
    assert!(!out.contains("Project packages:"), "{out}");
    assert_eq!(code, 0);
}

#[test]
fn cli_package_trust_uses_project_trust_extensions_for_package_commands() {
    let world = World::new(Some(json!({"trusted": "yes"})));
    let (out, err, code) = world.run(&["list"]);
    assert!(out.contains("Project packages:"), "{out}\n{err}");
    assert!(out.contains("npm:@project/pkg"), "{out}");
    assert!(!out.contains("No packages installed."), "{out}");
    assert_eq!(err, "");
    assert_eq!(code, 0);
    let events = world.trust_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(
        events[0]
            .to_string()
            .contains(&format!("\"cwd\":\"{}\"", world.root.join("project").display())),
        "{events:?}"
    );
    // Not remembered.
    assert_eq!(world.trust_file(), None);
}

#[test]
fn cli_package_trust_remembers_a_decision_when_asked() {
    let world = World::new(Some(json!({"trusted": "no", "remember": true})));
    let (out, _, _) = world.run(&["list"]);
    assert!(out.contains("No packages installed."), "{out}");
    let saved = world.trust_file().expect("trust.json written");
    assert!(
        saved
            .to_string()
            .contains(&world.root.join("project").display().to_string())
            && saved.to_string().contains("false"),
        "{saved}"
    );
    // The saved decision answers the next run without asking again.
    let before = world.trust_events().len();
    let (out, _, _) = world.run(&["list"]);
    assert!(out.contains("No packages installed."), "{out}");
    assert_eq!(
        world.trust_events().len(),
        before + 1,
        "a decision of no still asks the hook first"
    );
}

#[test]
fn cli_package_trust_undecided_falls_through_to_the_default() {
    let world = World::new(Some(json!({"trusted": "undecided"})));
    std::fs::write(
        world.root.join("agent/settings.json"),
        json!({"defaultProjectTrust": "always"}).to_string(),
    )
    .unwrap();
    let (out, _, _) = world.run(&["list"]);
    assert!(out.contains("Project packages:"), "{out}");
    assert_eq!(world.trust_events().len(), 1);
}

#[test]
fn cli_package_trust_handler_errors_and_load_failures_are_warnings() {
    let world = World::new(Some(Value::Null));
    let broken = world.root.join("agent/extensions/broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(
        broken.join("extension.json"),
        json!({"name": "broken", "command": "/nonexistent/extension"}).to_string(),
    )
    .unwrap();
    let (out, err, code) = world.run(&["list"]);
    assert!(out.contains("No packages installed."), "{out}");
    let manifest = world.root.join("agent/extensions/trust/extension.json");
    assert!(
        err.contains(&format!(
            "Warning: Extension \"{}\" project_trust error: project_trust handler returned no decision\n",
            manifest.display()
        )),
        "{err}"
    );
    assert!(
        err.contains(&format!(
            "Warning: Failed to load extension \"{}\": ",
            broken.join("extension.json").display()
        )),
        "{err}"
    );
    assert_eq!(code, 0);
}

#[test]
fn cli_package_trust_update_and_overrides_never_ask_extensions() {
    let world = World::new(Some(json!({"trusted": "yes"})));
    std::fs::write(
        world.root.join("agent/settings.json"),
        json!({"defaultProjectTrust": "always"}).to_string(),
    )
    .unwrap();
    world.run(&["update", "--extensions"]);
    assert!(world.trust_events().is_empty(), "update uses saved trust only");
    let (out, _, _) = world.run(&["list", "--no-approve"]);
    assert!(out.contains("No packages installed."), "{out}");
    let (out, _, _) = world.run(&["list", "--approve"]);
    assert!(out.contains("Project packages:"), "{out}");
    assert!(world.trust_events().is_empty(), "an override decides alone");
}

#[test]
fn cli_package_trust_applies_to_local_package_and_config_changes() {
    let untrusted = World::new(Some(json!({"trusted": "no"})));
    let (_, err, code) = untrusted.run(&["install", "./x", "-l"]);
    assert_eq!(
        err,
        "Project is not trusted. Use --approve to modify local package config.\n"
    );
    assert_eq!(code, 1);
    let (_, err, code) = untrusted.run(&["config", "-l"]);
    assert_eq!(
        err,
        "Project is not trusted. Use --approve to modify local resource config.\n"
    );
    assert_eq!(code, 1);
    assert_eq!(untrusted.trust_events().len(), 2);
    let trusted = World::new(Some(json!({"trusted": "yes"})));
    let (_, err, _) = trusted.run(&["config", "-l"]);
    assert!(!err.contains("Project is not trusted"), "{err}");
    assert_eq!(trusted.trust_events().len(), 1);
}

#[test]
fn cli_package_trust_asks_no_extension_when_nothing_needs_trust() {
    let world = World::new(Some(json!({"trusted": "no", "remember": true})));
    std::fs::remove_dir_all(world.root.join("project/.pi")).unwrap();
    let (out, err, code) = world.run(&["list"]);
    assert!(out.contains("No packages installed."), "{out}");
    assert_eq!(err, "");
    assert_eq!(code, 0);
    assert!(world.trust_events().is_empty(), "nothing to trust: no hook runs");
    assert_eq!(world.trust_file(), None);
}

#[test]
fn cli_package_trust_config_reports_hook_warnings() {
    let world = World::new(Some(Value::Null));
    let (_, err, code) = world.run(&["config", "-l"]);
    let manifest = world.root.join("agent/extensions/trust/extension.json");
    assert_eq!(
        err,
        format!(
            "Warning: Extension \"{}\" project_trust error: project_trust handler returned no decision\nProject is not trusted. Use --approve to modify local resource config.\n",
            manifest.display()
        )
    );
    assert_eq!(code, 1);
}
