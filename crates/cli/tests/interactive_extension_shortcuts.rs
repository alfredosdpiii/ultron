//! Extensions in a real terminal (tmux): the bundled example extension (crates/extensions/examples/hello) binds
//! Alt+H, which runs its handler in the session (its `ui/notify` reaches the screen) and `/hotkeys` lists under
//! **Extensions**; a handler that fails shows `Shortcut handler error: <message>`; a provider an extension declares
//! is offered by `/model` and reached by the scoped Ctrl+P cycle, and choosing it sends the next prompt to the
//! provider's base URL.

mod tui_harness;

use serde_json::json;
use std::path::Path;
use tui_harness::{Mock, TempDir, Tui, text_reply};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];
const QUIET: [(&str, &str); 2] = [("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")];

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

#[test]
fn cli_interactive_example_extension_shortcut_runs_its_handler() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/examples/hello");
    let dir = tmp.0.join("agent/extensions/hello");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(example.join("hello.py"), dir.join("hello.py")).unwrap();
    // The example's manifest, run with the tests' interpreter.
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(example.join("extension.json")).unwrap()).unwrap();
    manifest["command"] = json!(python());
    std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
    let tui = Tui::start_at(&tmp.0, mock.port, 100, 40, &MODEL, &QUIET);
    tui.wait("hello: startup");
    tui.send_keys(&["M-h"]);
    tui.wait("Hello from a shortcut");
    tui.send_text("/hotkeys");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Say hello");
    let extensions = screen.find("Extensions").expect("an Extensions section");
    assert!(screen[extensions..].contains("Alt+H"), "{screen}");
    drop(tui);
}

/// The probe extension declaring the `ext-proxy` provider whose one model is served at `port`.
fn declare_provider(root: &Path, port: u16) {
    let dir = root.join("agent/extensions/probe");
    std::fs::create_dir_all(&dir).unwrap();
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py");
    let providers = json!([{
        "name": "ext-proxy",
        "baseUrl": format!("http://127.0.0.1:{port}"),
        "api": "anthropic-messages",
        "apiKey": "ext-key",
        "models": [{"id": "proxied-model", "name": "Proxied Model", "reasoning": false, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 50000, "maxTokens": 2000}],
    }]);
    std::fs::write(
        dir.join("extension.json"),
        json!({
            "name": "probe",
            "command": python(),
            "args": ["-I", probe.to_string_lossy()],
            "env": {"PROBE_CAPABILITIES": json!({"providers": providers}).to_string()},
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn cli_interactive_model_selector_offers_an_extension_provider() {
    // The built-in provider's server has no reply of its own: the answer can only come from the extension's.
    let mock = Mock::start(Vec::new(), None);
    let provider = Mock::start(vec![text_reply("Answered by the extension provider.")], None);
    let tmp = TempDir::new();
    declare_provider(&tmp.0, provider.port);
    let tui = Tui::start_at(&tmp.0, mock.port, 100, 40, &MODEL, &QUIET);
    tui.wait("Session:");
    tui.send_text("/model proxied");
    tui.send_keys(&["Enter"]);
    tui.wait("proxied-model");
    tui.send_keys(&["Enter"]);
    tui.wait("ext-proxy");
    tui.send_text("which model");
    tui.send_keys(&["Enter"]);
    tui.wait("Answered by the extension provider.");
    drop(tui);
}

#[test]
fn cli_interactive_scoped_model_cycle_reaches_an_extension_provider() {
    let mock = Mock::start(Vec::new(), None);
    let provider = Mock::start(vec![text_reply("Cycled to the extension provider.")], None);
    let tmp = TempDir::new();
    declare_provider(&tmp.0, provider.port);
    // The scope Ctrl+P cycles in: the settings' `enabledModels`.
    let sonnet = "anthropic/claude-sonnet-4-5-20250929";
    std::fs::write(
        tmp.0.join("agent/settings.json"),
        json!({"enabledModels": [sonnet, "ext-proxy/proxied-model"]}).to_string(),
    )
    .unwrap();
    let tui = Tui::start_at(&tmp.0, mock.port, 100, 40, &["--model", sonnet], &QUIET);
    tui.wait("Session:");
    tui.send_keys(&["C-p"]);
    tui.wait("Switched to Proxied Model");
    tui.send_text("which model");
    tui.send_keys(&["Enter"]);
    tui.wait("Cycled to the extension provider.");
    drop(tui);
}

#[test]
fn cli_interactive_extension_shortcut_failure_is_shown() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let dir = tmp.0.join("agent/extensions/probe");
    std::fs::create_dir_all(&dir).unwrap();
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py");
    std::fs::write(
        dir.join("extension.json"),
        json!({
            "name": "probe",
            "command": python(),
            "args": ["-I", probe.to_string_lossy()],
            "env": {
                "PROBE_CAPABILITIES": json!({"shortcuts": [{"key": "alt+e", "description": "Fails"}]}).to_string(),
                "PROBE_SHORTCUT_ERRORS": json!({"alt+e": "nothing to do here"}).to_string(),
            },
        })
        .to_string(),
    )
    .unwrap();
    let tui = Tui::start_at(&tmp.0, mock.port, 100, 40, &MODEL, &QUIET);
    tui.wait("Session:");
    tui.send_keys(&["M-e"]);
    tui.wait("Error: Shortcut handler error: nothing to do here");
    drop(tui);
}
