//! The omp-inspired overlays of the Rust TUI in a real terminal (tmux), with the additions on (no
//! `ULTRON_TUI_PARITY`): F5 retries a failed turn, Alt+L resets the display, Alt+A opens the agent hub, `/copy`
//! opens the copy picker, Esc-Esc the rewind selector (which forks before the picked turn), Ctrl+L the model hub.
//! These have no TypeScript counterpart; expectations are the behavior the overlays promise.

mod tui_harness;

use serde_json::json;
use tui_harness::{Mock, Reply, Tui, sse, text_reply};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];
const QUIET: [(&str, &str); 2] = [("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")];

/// An Anthropic stream that fails mid-turn (an `error` event).
fn error_reply() -> Reply {
    let start = json!({"type": "message_start", "message": {"id": "msg_e", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}});
    Reply {
        chunks: sse(&[
            start,
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ]),
        hold_after: None,
    }
}

fn start(mock: &Mock) -> Tui {
    let tui = Tui::start(mock.port, 100, 30, &MODEL, &QUIET);
    tui.wait("Session:");
    tui
}

fn submit(tui: &Tui, text: &str, wait: &str) -> String {
    tui.send_text(text);
    tui.send_keys(&["Enter"]);
    tui.wait(wait)
}

#[test]
fn cli_interactive_f5_retries_a_failed_turn() {
    let mock = Mock::start(vec![error_reply(), text_reply("Recovered answer.")], None);
    let tui = start(&mock);
    submit(&tui, "please work", "Overloaded");
    tui.send_keys(&["F5"]);
    let screen = tui.wait("Recovered answer.");
    assert!(screen.contains("please work"), "{screen}");
    // Nothing failed any more: F5 says so.
    tui.send_keys(&["F5"]);
    tui.wait("Nothing to retry");
}

#[test]
fn cli_interactive_alt_l_repaints_the_display() {
    let mock = Mock::start(vec![text_reply("Still here.")], None);
    let tui = start(&mock);
    submit(&tui, "hello", "Still here.");
    // Scribble over the terminal behind the TUI's back; Alt+L paints everything again.
    tui.tmux(&["send-keys", "-t", "t", "-l", ""]);
    tui.tmux(&["clear-history", "-t", "t"]);
    tui.send_keys(&["M-l"]);
    let screen = tui.wait("Still here.");
    assert!(screen.contains("hello") && screen.contains("ultron v"), "{screen}");
}

#[test]
fn cli_interactive_alt_a_opens_the_agent_hub() {
    let mock = Mock::start(Vec::new(), None);
    let tui = start(&mock);
    tui.send_keys(&["M-a"]);
    tui.wait("No agents in this session");
    tui.send_keys(&["Escape"]);
    let screen = tui.wait("Session:");
    assert!(!screen.contains("No agents in this session"), "{screen}");
}

#[test]
fn cli_interactive_copy_picker_copies_the_selected_turn() {
    let mock = Mock::start(vec![text_reply("Copy **this** answer.")], None);
    let tui = start(&mock);
    tui.tmux(&["set-option", "-s", "set-clipboard", "on"]);
    submit(&tui, "hi", "Copy this answer.");
    submit(&tui, "/copy", "pick what to put on the clipboard");
    tui.send_keys(&["Enter"]);
    tui.wait("Copied");
    assert!(
        tui.tmux(&["show-buffer"]).contains("Copy **this** answer."),
        "{}",
        tui.tmux(&["show-buffer"])
    );
}

#[test]
fn cli_interactive_esc_esc_rewinds_to_a_new_session_before_the_turn() {
    let mock = Mock::start(vec![text_reply("First."), text_reply("Second.")], None);
    let tui = start(&mock);
    submit(&tui, "first ask", "First.");
    submit(&tui, "second ask", "Second.");
    // A quick double tap may arrive as one read (Alt+Escape); either way it opens the selector.
    tui.send_keys(&["Escape", "Escape"]);
    let picker = tui.wait("pick the point to continue from");
    // The last turn is selected; Left jumps to its user turn.
    tui.send_keys(&["Left"]);
    tui.wait_change(&picker);
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Forked to new session");
    // The fork holds the path before the picked turn; its prompt is back in the editor.
    assert!(!screen.contains("Second."), "{screen}");
    assert!(screen.lines().any(|l| l.trim() == "second ask"), "{screen}");
}

#[test]
fn cli_interactive_ctrl_l_opens_the_model_hub_and_switches_models() {
    let mock = Mock::start(Vec::new(), None);
    let tui = start(&mock);
    tui.send_keys(&["C-l"]);
    tui.wait("Models");
    tui.send_text("claude-sonnet-4-5-20250929");
    tui.wait("❯ anthropic/claude-sonnet-4-5-20250929");
    // Enter opens the reasoning model's thinking strip; Enter again uses the model at the strip's level.
    tui.send_keys(&["Enter"]);
    tui.wait("thinking for claude-sonnet-4-5-20250929:");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Selected anthropic/claude-sonnet-4-5-20250929.");
    assert!(screen.contains("claude-sonnet-4-5-20250929"), "{screen}");
}

/// The session worker dies under the TUI: it attaches again (a fresh worker on the intact session file) and says
/// so with the TypeScript client's words; a second death within 30 s is reported, not recovered.
#[test]
fn cli_interactive_reattaches_after_the_session_worker_dies() {
    let mock = Mock::start(vec![text_reply("Before."), text_reply("After.")], None);
    let tui = Tui::start(
        mock.port,
        100,
        30,
        &MODEL,
        &[("ULTRON_AUTO_NAME", "off"), ("ULTRON_TUI_PARITY", "1")],
    );
    tui.wait("Session:");
    submit(&tui, "first", "Before.");
    let kill = || {
        let pattern = format!("session-worker.*{}", tui.root().display());
        std::process::Command::new("pkill")
            .args(["-KILL", "-f", &pattern])
            .status()
            .unwrap();
    };
    kill();
    let screen = tui.wait("was restarted");
    assert!(
        screen.contains("The session worker stopped unexpectedly and was restarted; the Session is intact"),
        "{screen}"
    );
    // The transcript is the session file's; the next turn runs in the new worker.
    assert!(screen.contains("first") && screen.contains("Before."), "{screen}");
    submit(&tui, "second", "After.");
    kill();
    let screen = tui.wait("stopped again");
    assert!(
        screen.contains("Error: The session worker stopped again; your session is saved"),
        "{screen}"
    );
}
