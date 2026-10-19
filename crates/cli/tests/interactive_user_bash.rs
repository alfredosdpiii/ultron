//! The TUI's `!`/`!!` commands run in the shell, as interactive-mode.ts `handleBashCommand` runs them without an
//! extension's `user_bash`, in a real terminal (tmux). A Pi extension in the profile changes nothing (Pi extensions
//! are not supported by ultron-rs).

mod tui_harness;

use tui_harness::{Mock, TempDir, Tui};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];
const QUIET: [(&str, &str); 2] = [("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")];

#[test]
fn cli_interactive_user_bash_commands_run_in_the_shell() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let dir = tmp.0.join("agent/extensions/hello");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("extension.json"),
        r#"{"name": "hello", "command": "/bin/false", "capabilities": {"events": ["user_bash"]}}"#,
    )
    .unwrap();
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 40, &MODEL, &QUIET);
    tui.wait("Session:");
    let cwd = tmp.0.join("cwd");
    tui.send_text("!!touch quiet && echo quiet-$((3+4))");
    tui.send_keys(&["Enter"]);
    tui.wait("quiet-7");
    assert!(cwd.join("quiet").exists());
    tui.send_text("!touch ran && echo local-$((40+2))");
    tui.send_keys(&["Enter"]);
    tui.wait("local-42");
    assert!(cwd.join("ran").exists());
    drop(tui);
}
