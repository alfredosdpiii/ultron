//! The agent hub in a real terminal (tmux) against a real session with a real `rlm.spawn` subagent: Alt+A lists
//! the running subagent, Enter opens its transcript (read from the session), submitting a line steers it
//! (`agent_message.send` with `steer: true`, the hub's notice says so), and the subagent's transcript then holds the
//! untrusted-message envelope and the answer it gave after it.

mod tui_harness;

use serde_json::json;
use tui_harness::{Mock, Reply, Tui, sse, text_reply};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];
const QUIET: [(&str, &str); 2] = [("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")];

fn tool_reply(code: &str) -> Reply {
    let start = json!({"type": "message_start", "message": {"id": "msg_t", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}});
    Reply {
        chunks: sse(&[
            start,
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "rlm", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"code": code}).to_string()}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
            json!({"type": "message_stop"}),
        ]),
        hold_after: None,
    }
}

/// A text answer held after its first chunk until the mock is released.
fn held(text: &str) -> Reply {
    Reply {
        hold_after: Some(1),
        ..text_reply(text)
    }
}

#[test]
fn cli_interactive_agent_hub_steers_a_running_subagent() {
    // The root's answer after its cell and the child's first answer race for replies 2 and 3: both are held, so
    // the child is running whichever it got; after the release, the steered child asks again (reply 4).
    let mock = Mock::start(
        vec![
            tool_reply("h = await rlm.spawn('Audit the tests', name='auditor')\nprint('spawned')"),
            held("Working on it."),
            held("Working on it."),
            text_reply("STEERED ANSWER"),
        ],
        None,
    );
    let tui = Tui::start(mock.port, 110, 34, &MODEL, &QUIET);
    tui.wait("Session:");
    tui.send_text("delegate the audit");
    tui.send_keys(&["Enter"]);
    tui.wait("spawned");

    tui.send_keys(&["M-a"]);
    tui.wait("running");
    tui.send_keys(&["Enter"]);
    tui.wait("Audit the tests");
    tui.send_text("focus on src");
    tui.send_keys(&["Enter"]);
    tui.send_keys(&["Escape"]);
    tui.wait("Steered \"");

    mock.release();
    // Reopening the transcript reads it again: the envelope, then the steered answer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let screen = loop {
        tui.send_keys(&["Enter"]);
        let screen = tui.wait("Audit the tests");
        if screen.contains("STEERED ANSWER") {
            break screen;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the steered answer never showed: {screen}"
        );
        tui.send_keys(&["Escape"]);
        tui.wait("Roster");
    };
    assert!(screen.contains("[Untrusted family message]"), "{screen}");
}
