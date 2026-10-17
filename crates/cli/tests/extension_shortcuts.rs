//! Extension shortcuts through the session (interactive-mode.ts `setupExtensionShortcuts` over runner.ts
//! `getShortcuts`): the TUI's effective keybindings resolve the extensions' shortcuts with TypeScript's conflict
//! outcomes (crates/extensions/fixtures/host.json, recorded from `ExtensionRunner.getShortcuts` over the default
//! keybindings), the session answers `extensionShortcuts` with them and its warnings, and `extensionShortcut` runs
//! the owning extension's `shortcuts/run`.

use interactive::extension_shortcuts::effective_keybindings;
use serde_json::{Value, json};
use session_core::core::{Input, Op};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;

const HANG: Duration = Duration::from_secs(120);

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../extensions/fixtures/host.json")).unwrap()
}

/// The keybindings the interactive TUI runs with (crates/cli `run_interactive`, no user overrides).
fn tui_keybindings() -> Vec<(String, Vec<String>)> {
    effective_keybindings(&KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    ))
}

/// The TypeScript scenarios over the default keybindings resolve the same under the Rust TUI's keybindings.
#[test]
fn cli_extension_shortcuts_resolve_under_the_tui_keybindings_like_typescript() {
    let keybindings = tui_keybindings();
    let mut checked = 0;
    for scenario in fixture()["shortcuts"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        // Scenarios that override the defaults are the conflict rules' own (crates/extensions host_fixture).
        let recorded: Vec<(String, Vec<String>)> = serde_json::from_value(scenario["keybindings"].clone()).unwrap();
        let defaults = &fixture()["shortcuts"][0]["keybindings"];
        if serde_json::to_value(&recorded).unwrap() != *defaults {
            continue;
        }
        let extensions: Vec<extensions::shortcuts::Registered> = scenario["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let path = e["path"].as_str().unwrap().to_string();
                let keys = e["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| {
                        let k = k.as_str().unwrap().to_string();
                        let description = Some(format!("{path} {k}"));
                        (k, description)
                    })
                    .collect();
                (path, keys)
            })
            .collect();
        let resolved = extensions::shortcuts::resolve(&extensions, &keybindings);
        let shortcuts: Vec<Value> = resolved
            .shortcuts
            .iter()
            .map(|(key, s)| json!({"key": key, "registered": s.key, "description": s.description, "extensionPath": s.extension_path}))
            .collect();
        let diagnostics: Vec<Value> = resolved
            .diagnostics
            .iter()
            .map(|(message, path)| json!({"message": message, "path": path}))
            .collect();
        assert_eq!(json!(shortcuts), scenario["shortcuts"], "{name}");
        assert_eq!(json!(diagnostics), scenario["diagnostics"], "{name}");
        checked += 1;
    }
    assert_eq!(checked, 5);
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn probe() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py")
}

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
    manifest: PathBuf,
    tx: Sender<Input>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl World {
    fn open() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (agent_dir, cwd) = (root.join("agent"), root.join("cwd"));
        std::fs::create_dir_all(&cwd).unwrap();
        let dir = agent_dir.join("extensions/probe");
        std::fs::create_dir_all(&dir).unwrap();
        let log = root.join("probe.jsonl");
        let capabilities = json!({"shortcuts": [
            {"key": "alt+h", "description": "Say hello"},
            {"key": "ctrl+c", "description": "Steal interrupt"},
            {"key": "ctrl+v", "description": "Paste differently"},
        ]});
        let manifest = dir.join("extension.json");
        std::fs::write(
            &manifest,
            json!({
                "name": "probe",
                "command": python(),
                "args": ["-I", probe().to_string_lossy()],
                "env": {
                    "PROBE_LOG": log.to_string_lossy(),
                    "PROBE_CAPABILITIES": capabilities.to_string(),
                    "PROBE_SHORTCUT_ERRORS": json!({"ctrl+v": "the clipboard is empty"}).to_string(),
                },
                "startupTimeoutMs": 120_000,
                "eventTimeoutMs": 120_000,
            })
            .to_string(),
        )
        .unwrap();
        let vars: Vec<(String, String)> = vec![
            ("HOME".into(), root.display().to_string()),
            ("ULTRON_CODING_AGENT_DIR".into(), agent_dir.display().to_string()),
            ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
            ("ANTHROPIC_API_KEY".into(), "test-key".into()),
            ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
            ("PI_OFFLINE".into(), "1".into()),
        ];
        let env = cli::Env {
            cwd: cwd.clone(),
            home: root.clone(),
            var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
            docs: config::paths::normalize(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"),
            ),
        };
        let sessions = agent_dir.join("experimental/sessions");
        let repo = session_journal::SessionRepo::new(&sessions, session_journal::SystemClock::shared());
        let metadata = repo
            .create(&cwd.display().to_string(), None, None)
            .unwrap()
            .metadata()
            .clone();
        let open = session_core::core::OpenOptions {
            env,
            flags: session_core::ModelFlags {
                model: Some("anthropic/claude-sonnet-4-5".into()),
                ..Default::default()
            },
            session_root: sessions,
            metadata,
            mode: "interactive".into(),
            services: cli::compose::Services::default(),
            auto_name: false,
        };
        let (tx, handle, _) = session_core::core::spawn(open, Box::new(|_| {})).unwrap();
        World {
            _tmp: tmp,
            root,
            log,
            manifest,
            tx,
            handle: Some(handle),
        }
    }

    fn call(&self, op: Op) -> Result<Option<Value>, String> {
        session_core::core::call(&self.tx, op)
    }

    fn shortcut_runs(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|e| e.get("shortcut").cloned())
            .collect()
    }

    fn close(mut self) {
        let (done, closed) = std::sync::mpsc::channel();
        self.tx.send(Input::Close(done)).unwrap();
        closed.recv_timeout(HANG).unwrap();
        self.handle.take().unwrap().join().unwrap();
        let _ = &self.root;
    }
}

#[test]
fn cli_extension_shortcuts_are_listed_and_run_through_the_session() {
    let world = World::open();
    let path = world.manifest.display().to_string();
    let keybindings = tui_keybindings();
    let listed = world
        .call(Op::ExtensionShortcuts {
            keybindings: keybindings.clone(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        listed,
        json!({
            "shortcuts": [
                {"key": "alt+h", "description": "Say hello", "extensionPath": path},
                {"key": "ctrl+v", "description": "Paste differently", "extensionPath": path},
            ],
            "diagnostics": [
                {"type": "warning", "message": format!("Extension shortcut 'ctrl+c' from {path} conflicts with built-in shortcut. Skipping."), "path": path},
                {"type": "warning", "message": format!("Extension shortcut conflict: 'ctrl+v' is built-in shortcut for app.clipboard.pasteImage and {path}. Using {path}."), "path": path},
            ],
        })
    );
    // The op crosses the worker's control service as JSON.
    let op = Op::ExtensionShortcut {
        key: "alt+h".into(),
        keybindings: keybindings.clone(),
    };
    assert_eq!(Op::from_json(&op.to_json()).unwrap().to_json(), op.to_json());
    assert_eq!(world.call(op).unwrap(), None);
    let start = Instant::now();
    while world.shortcut_runs().is_empty() {
        assert!(start.elapsed() < HANG, "the shortcut never reached the extension");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(world.shortcut_runs(), [json!({"key": "alt+h"})]);
    // A refused key is not the extension's.
    assert_eq!(
        world.call(Op::ExtensionShortcut {
            key: "ctrl+c".into(),
            keybindings,
        }),
        Err("No extension shortcut ctrl+c".to_string())
    );
    assert_eq!(world.shortcut_runs().len(), 1);
    // A handler's failure is the answer (the TUI shows it as `Shortcut handler error: <message>`).
    assert_eq!(
        world.call(Op::ExtensionShortcut {
            key: "ctrl+v".into(),
            keybindings: tui_keybindings(),
        }),
        Err("the clipboard is empty".to_string())
    );
    world.close();
}

/// The composition root's `KeybindingsManager.create(agentDir)`: `<agentDir>/keybindings.json` migrated and kept to
/// string bindings; a missing or malformed file is no bindings.
#[test]
fn cli_user_keybindings_load_the_agent_dir_file() {
    let dir = std::env::temp_dir().join(format!("cli-user-keybindings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let linux = tui::keybindings::Platform::new("linux", false);
    assert!(cli::interactive::user_keybindings(&dir, linux).is_empty());
    std::fs::write(
        dir.join("keybindings.json"),
        r#"{"app.clear": 1, "exit": "ctrl+q", "zzz": ["a"]}"#,
    )
    .unwrap();
    assert_eq!(
        cli::interactive::user_keybindings(&dir, linux),
        [
            ("app.exit".to_string(), Some(vec!["ctrl+q".to_string()])),
            ("zzz".to_string(), Some(vec!["a".to_string()])),
        ]
    );
    std::fs::write(dir.join("keybindings.json"), "{broken").unwrap();
    assert!(cli::interactive::user_keybindings(&dir, linux).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
