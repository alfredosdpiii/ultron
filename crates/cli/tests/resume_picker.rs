//! `ultron -r <prompt>` on a terminal (native-command.ts `selectedSessionId`: `parsed.resume && stdin.isTTY &&
//! stdout.isTTY` asks `selectInteractiveSession` before the prompt runs): the numbered list, the chosen session
//! continued with the prompt, its answer streamed (no `-p`). Driven in tmux; the provider is a local mock.

mod tui_harness;

use std::process::Command;
use tui_harness::{Mock, TempDir, Tui, text_reply};

const MODEL: &str = "anthropic/claude-sonnet-4-5";

fn session_texts(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in walk(dir) {
        if entry.extension().is_some_and(|e| e == "jsonl") {
            out.push(std::fs::read_to_string(entry).unwrap());
        }
    }
    out
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn cli_native_prompted_resume_on_a_terminal_asks_which_session() {
    let mock = Mock::start(
        vec![
            text_reply("alpha answer"),
            text_reply("beta answer"),
            text_reply("picked answer"),
        ],
        None,
    );
    let tmp = TempDir::new();
    let root = tmp.0.canonicalize().unwrap();
    let agent = root.join("agent");
    for dir in [&agent, &root.join("cwd"), &root.join("server")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(
        agent.join("models.json"),
        format!(
            r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{}"}}}}}}"#,
            mock.port
        ),
    )
    .unwrap();
    for prompt in ["alpha prompt", "beta prompt"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ultron"))
            .args(["-p", "--model", MODEL, prompt])
            .env_clear()
            .current_dir(root.join("cwd"))
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("ULTRON_CODING_AGENT_DIR", &agent)
            .env("ULTRON_SERVER_DIR", root.join("server"))
            .env("ANTHROPIC_API_KEY", "test-key")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let tui = Tui::start_at(&root, mock.port, 100, 30, &["-r", "--model", MODEL, "follow up"], &[]);
    let screen = tui.wait("Select a session number:");
    let listed: Vec<&str> = screen.lines().filter(|l| l.starts_with("  ")).collect();
    assert_eq!(listed.len(), 2, "{screen}");
    assert!(screen.contains("Available sessions:"), "{screen}");
    // Newest first: 2 is the alpha session.
    tui.send_text("2");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("<exited>");
    assert!(screen.contains("picked answer"), "{screen}");
    drop(tui);
    let texts = session_texts(&agent.join("experimental").join("sessions"));
    assert_eq!(texts.len(), 2);
    let alpha = texts.iter().find(|t| t.contains("alpha prompt")).unwrap();
    let beta = texts.iter().find(|t| t.contains("beta prompt")).unwrap();
    assert!(alpha.contains("follow up"), "the picked session is continued");
    assert!(!beta.contains("follow up"));
}
