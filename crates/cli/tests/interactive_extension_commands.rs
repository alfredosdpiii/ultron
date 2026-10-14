//! An extension command run from the TUI (`directExtensionCommand`) runs until its handler returns, without holding
//! up the session's other commands: in a real terminal (tmux), `/slow` reaches an extension process that answers
//! only once released, and meanwhile a prompt still runs its turn.

mod tui_harness;

use serde_json::json;
use std::time::{Duration, Instant};
use tui_harness::{Mock, TempDir, Tui, text_reply};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];
const QUIET: [(&str, &str); 2] = [("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")];

/// Registers `/slow`; `commands/run` writes `started`, then answers once `release` exists.
const EXTENSION: &str = r#"import json, os, sys, time
def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if "id" not in message or method is None:
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"protocolVersion": "2026-10-10",
            "extensionInfo": {"name": "slow"}, "capabilities": {"commands": [{"name": "slow", "description": "Takes a while"}]}}})
    elif method == "commands/run":
        open("started", "w").close()
        while not os.path.exists("release"):
            time.sleep(0.02)
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
        break
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "Method not found"}})
"#;

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cli_interactive_an_extension_command_does_not_hold_up_other_commands() {
    let mock = Mock::start(vec![text_reply("Answered while the command runs.")], None);
    let tmp = TempDir::new();
    let dir = tmp.0.join("agent/extensions/slow");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ext.py"), EXTENSION).unwrap();
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    std::fs::write(
        dir.join("extension.json"),
        json!({"name": "slow", "command": python, "args": ["ext.py"]}).to_string(),
    )
    .unwrap();
    let tui = Tui::start_at(&tmp.0, mock.port, 100, 30, &MODEL, &QUIET);
    tui.wait("Session:");
    tui.send_text("/slow");
    tui.send_keys(&["Enter"]);
    wait_for("the command reached the extension", || dir.join("started").exists());
    tui.send_text("are you there");
    tui.send_keys(&["Enter"]);
    tui.wait("Answered while the command runs.");
    assert!(!dir.join("release").exists());
    std::fs::write(dir.join("release"), "").unwrap();
    drop(tui);
}
