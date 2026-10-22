//! Entry-point parity of the interactive TUI (Rust-only fixes, literal expectations), in a real terminal (tmux on a
//! private socket), against a scripted 127.0.0.1 Anthropic server with private HOME / agent / server dirs:
//!
//! - a resumed session shows its own thinking level (the worker's), and `--thinking` on resume changes it;
//! - `--models` scopes Ctrl+P as the worker's model cycling does;
//! - `/persona` in `ultron durable` reports the worker's effective persona (the durable default is on), and
//!   `/persona none` turns it off for good (a removed key would bring the default back);
//! - `/settings` -> Models in `ultron durable` lists the durable default model (`durable.model`, the Claude
//!   subscription by default) where its sessions save a chosen model; plain `ultron` does not.

mod tui_harness;

use std::process::{Command, Stdio};
use tui_harness::{Mock, TempDir, Tui, text_reply};

const MODEL: &str = "anthropic/claude-sonnet-4-5";

/// `ultron <args>` in `root` with the harness's private layout (what [`Tui::start_at`] gives the TUI).
fn run(root: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(args)
        .current_dir(root.join("cwd"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("ULTRON_CODING_AGENT_DIR", root.join("agent"))
        .env("ULTRON_SERVER_DIR", root.join("server"))
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The harness's layout in `root` (agent dir pointing at `port`), for a run before the TUI starts.
fn layout(root: &std::path::Path, port: u16) {
    for dir in ["agent", "cwd", "server"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(
        root.join("agent/models.json"),
        format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
    )
    .unwrap();
}

const SESSION: &str = "0b9c3f7e-1d2a-4c5b-8e6f-7a8b9c0d1e2f";

#[test]
fn cli_entry_tui_resumed_session_shows_its_own_thinking_level_and_thinking_flag_applies_on_resume() {
    let mock = Mock::start(vec![text_reply("First."), text_reply("Second.")], None);
    let tmp = TempDir::new();
    layout(&tmp.0, mock.port);
    let out = run(
        &tmp.0,
        &[
            "--model",
            MODEL,
            "--thinking",
            "high",
            "--session-id",
            SESSION,
            "-p",
            "one",
        ],
        &[],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Resumed without --thinking: the session's own level, not the "medium" a new session would start with.
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 30, &["--session", SESSION], &[]);
    tui.wait("claude-sonnet-4-5 • high");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    drop(tui);
    // Resumed with --thinking low: the flag holds for the resumed session.
    let tui = Tui::start_at(
        &tmp.0,
        mock.port,
        120,
        30,
        &["--session", SESSION, "--thinking", "low"],
        &[],
    );
    tui.wait("claude-sonnet-4-5 • low");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}

#[test]
fn cli_entry_tui_models_flag_scopes_ctrl_p() {
    let mock = Mock::start(Vec::new(), None);
    let tui = Tui::start(mock.port, 120, 30, &["--model", MODEL, "--models", MODEL], &[]);
    tui.wait("claude-sonnet-4-5");
    tui.send_keys(&["C-p"]);
    // One model in the --models scope: nothing to switch to (without the scope every Anthropic model is a step).
    tui.wait("Only one model in scope");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}

#[test]
fn cli_entry_tui_durable_persona_reports_the_workers_state_and_none_sticks() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let durable = tmp.0.join(".ultron/server");
    let durable_env = durable.display().to_string();
    // The durable instance's own directory (HOME's .ultron/server): its sessions have the persona on by default.
    let env = [("ULTRON_SERVER_DIR", durable_env.as_str())];
    let tui = Tui::start_at(&tmp.0, mock.port, 140, 30, &["durable"], &env);
    tui.wait("ultron v");
    tui.send_text("/persona");
    tui.send_keys(&["Enter"]);
    tui.wait("Persona for this session follows the settings (on); keep-going off");
    tui.send_text("/persona none");
    // The argument completion takes the first Enter (it completes `none`); the second submits. Its list is open
    // once the text is typed but can be drawn late on a loaded machine: an Enter taken by the list before it was
    // drawn leaves the screen as it was, so wait for the list, whose closing is the change to wait for.
    let before = tui.wait("→ none");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait_change(&before);
    if !screen.contains("Persona off from the next message") {
        tui.send_keys(&["Enter"]);
    }
    tui.wait("Persona off from the next message");
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.0.join("agent/settings.json")).unwrap()).unwrap();
    assert_eq!(settings["persona"], "none");
    tui.send_text("/persona");
    tui.send_keys(&["Enter"]);
    tui.wait("Persona for this session follows the settings (off); keep-going off");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    // The durable server outlives its clients: stop it.
    let out = run(&tmp.0, &["durable", "stop"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    drop(tui);
}

/// `ultron-s -r` on a terminal is the session picker over the durable instance's sessions (the selector is
/// forwarded to the client, which runs in the home directory): both sessions `ultron-s --new` made are listed,
/// newest first, and the one picked opens in the TUI with its own conversation.
#[test]
fn cli_entry_tui_ultron_s_resume_opens_the_picked_durable_session() {
    let mock = Mock::start(vec![text_reply("Answer one."), text_reply("Answer two.")], None);
    let tmp = TempDir::new();
    layout(&tmp.0, mock.port);
    let durable = tmp.0.join(".ultron/server");
    let durable_env = durable.display().to_string();
    let env = [("ULTRON_SERVER_DIR", durable_env.as_str()), ("ULTRON_AUTO_NAME", "off")];
    let link = tmp.0.join("bin/ultron-s");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ultron"), &link).unwrap();
    let mut ids = Vec::new();
    for prompt in ["one", "two"] {
        let out = Command::new(&link)
            .args(["--new", "--model", MODEL, "-p", prompt])
            .current_dir(tmp.0.join("cwd"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &tmp.0)
            .env("ULTRON_CODING_AGENT_DIR", tmp.0.join("agent"))
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        ids.push(
            std::fs::read_to_string(durable.join("home-session"))
                .unwrap()
                .trim()
                .to_string(),
        );
    }
    assert_ne!(ids[0], ids[1]);
    let models = format!(
        r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{}"}}}}}}"#,
        mock.port
    );
    let tui = Tui::start_program(&tmp.0, &link, &models, 140, 30, &["-r"], &env);
    tui.wait("Select a session number:");
    // Newest first: the second session, then the first (the raw screen: the normalized one hides ids).
    let picker = tui.tmux(&["capture-pane", "-p", "-t", "t"]);
    let (newest, oldest) = (
        picker.find(&format!("1. {}", ids[1])),
        picker.find(&format!("2. {}", ids[0])),
    );
    assert!(newest.is_some() && oldest.is_some(), "{picker}");
    tui.send_text("2");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Answer one.");
    assert!(!screen.contains("Answer two."), "{screen}");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    let out = run(&tmp.0, &["durable", "stop"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    drop(tui);
}

/// Back from the Models list to the editor (one Escape per level, each taken before the next), then `/quit`.
fn close_and_quit(tui: &Tui, models: String) {
    tui.send_keys(&["Escape"]);
    let settings = tui.wait_change(&models);
    tui.send_keys(&["Escape"]);
    tui.wait_change(&settings);
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}

/// Open `/settings` -> Models and return the Models list.
fn models_list(tui: &Tui) -> String {
    tui.wait("ultron v");
    tui.send_text("/settings");
    tui.send_keys(&["Enter"]);
    // The settings list opens on Models (its first row): wait for it before Enter opens it.
    tui.wait("Models of the session, inference frames");
    tui.send_keys(&["Enter"]);
    tui.wait("Session model")
}

#[test]
fn cli_entry_tui_durable_settings_list_the_durable_default_model() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    // Plain `ultron`: no durable row; the session model's saved default is the global one.
    let plain = Tui::start_at(&tmp.0, mock.port, 160, 40, &["--model", MODEL], &[]);
    let screen = models_list(&plain);
    assert!(!screen.contains("Durable default model"), "{screen}");
    close_and_quit(&plain, screen);
    drop(plain);
    let durable = tmp.0.join(".ultron/server").display().to_string();
    let env = [("ULTRON_SERVER_DIR", durable.as_str())];
    let tui = Tui::start_at(&tmp.0, mock.port, 160, 40, &["durable", "--model", MODEL], &env);
    let screen = models_list(&tui);
    assert!(screen.contains("Durable default model"), "{screen}");
    assert!(screen.contains("claude-code/claude-opus-5-5 (default)"), "{screen}");
    close_and_quit(&tui, screen);
    let out = run(&tmp.0, &["durable", "stop"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    drop(tui);
}
