//! The TUI's `!`/`!!` commands go through the extensions' `user_bash` first, as interactive-mode.ts
//! `handleBashCommand` does, with the example extension (crates/extensions/examples/hello) in a real terminal (tmux):
//! a command it answers shows its result and is recorded (never run); a command it refuses fails closed (nothing
//! runs; only `Extension "<path>" error: <message>` is shown, `showExtensionError`); anything else runs as usual.

mod tui_harness;

use serde_json::json;
use std::path::Path;
use tui_harness::{Mock, TempDir, Tui};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];
const QUIET: [(&str, &str); 2] = [("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")];

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

#[test]
fn cli_interactive_user_bash_extension_answers_refuses_or_lets_commands_run() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/examples/hello");
    let dir = tmp.0.join("agent/extensions/hello");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(example.join("hello.py"), dir.join("hello.py")).unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(example.join("extension.json")).unwrap()).unwrap();
    manifest["command"] = json!(python());
    std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 40, &MODEL, &QUIET);
    tui.wait("hello: startup");
    let cwd = tmp.0.join("cwd");

    // Answered by the extension, with the event's fields; it never runs (no `answered` file).
    tui.send_text("!hello world; touch answered");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("(included) in");
    assert!(
        screen.contains(r#"hello ran "hello world; touch answered" (included) in"#),
        "{screen}"
    );
    tui.send_text("!!hello quietly");
    tui.send_keys(&["Enter"]);
    tui.wait(r#"hello ran "hello quietly" (excluded) in"#);
    assert!(!cwd.join("answered").exists());

    // Refused: the error is shown and nothing runs or is recorded.
    tui.send_text("!rm -rf nowhere; touch refused");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("hello refuses rm -rf");
    assert!(screen.contains(r#"Extension ""#), "{screen}");
    assert!(screen.contains(r#"" error: hello refuses rm -rf"#), "{screen}");
    assert!(!screen.contains("Error: hello refuses"), "no status error: {screen}");
    assert!(!screen.contains("$ rm -rf"), "nothing recorded: {screen}");

    // Not the extension's: it runs.
    tui.send_text("!touch ran && echo local-$((40+2))");
    tui.send_keys(&["Enter"]);
    tui.wait("local-42");
    assert!(cwd.join("ran").exists());
    assert!(!cwd.join("refused").exists());
    drop(tui);
}
