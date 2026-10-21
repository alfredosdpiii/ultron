//! `ultron --claude -c` (session-worker.ts, after `harness.lane("main")`): a resumed session whose lane is not on the
//! Claude root's provider moves to the root model (`lane.setModel`, the thinking level kept); one already on it, and
//! a plain `--model` on resume (recorded: print-continue-keeps-lane-model), keep the stored lane. The Rust binary,
//! daemon and worker, a FAKE `claude` (crates/ai/fixtures/claude-subscription/claude.py; PATH is only its directory
//! plus /usr/bin:/bin) and one fake upstream on loopback for both the Anthropic API and the subscription.

mod common;
mod executable_support;

use common::{Naming, Sandbox, assert_private_home, mock_server};
use serde_json::{Value, json};
use std::path::Path;

fn text_reply(text: &str) -> String {
    let usage = json!({"input_tokens": 20, "output_tokens": 1});
    [
        json!({"type": "message_start", "message": {"id": "msg_resume", "type": "message", "role": "assistant", "model": "m", "content": [], "stop_reason": null, "usage": usage}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

struct Root {
    sandbox: Sandbox,
    wrapper: std::path::PathBuf,
}

impl Root {
    fn new(port: u16) -> Root {
        let sandbox = Sandbox::new(port);
        assert_private_home(&sandbox.root);
        std::fs::write(
            sandbox.agent_dir.join("settings.json"),
            r#"{"sessionNaming":{"enabled":false}}"#,
        )
        .unwrap();
        std::fs::write(
            sandbox.agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}},"claude-subscription":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#
            ),
        )
        .unwrap();
        let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ai/fixtures/claude-subscription/claude.py");
        let wrapper = sandbox.root.join("fake-bin/claude");
        std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        executable_support::install(
            &wrapper,
            &format!("#!/bin/sh\nexec {python} -I {} \"$@\"\n", fixture.display()),
        );
        std::fs::create_dir_all(sandbox.root.join("tmp")).unwrap();
        Root { sandbox, wrapper }
    }

    /// `ultron <flags> -p <prompt>`; its exit code and stderr.
    fn run(&self, flags: &[&str], prompt: &str) -> (Option<i32>, String) {
        let mut command = self.sandbox.ultron();
        command
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.wrapper.parent().unwrap().display()),
            )
            .env("ULTRON_CLAUDE_COMMAND", &self.wrapper)
            .env("FAKE_LOG", self.sandbox.root.join("claude-log.jsonl"))
            .env("TMPDIR", self.sandbox.root.join("tmp"))
            .stdin(std::process::Stdio::null())
            .args(flags)
            .args(["-p", prompt]);
        let output = command.output().unwrap();
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// The stored lane configuration of the one session (its last `pi.lane.config` write).
    fn lane(&self) -> Value {
        let files = common::session_files(&self.sandbox.sessions_dir());
        assert_eq!(files.len(), 1, "{files:?}");
        let text = std::fs::read_to_string(&files[0]).unwrap();
        let mut lane = Value::Null;
        for line in text.lines() {
            let record: Value = serde_json::from_str(line).unwrap();
            for r in record.as_array().cloned().unwrap_or_else(|| vec![record.clone()]) {
                if r["namespace"] == "pi.lane.config" && r["key"] == "main" {
                    lane = r["value"].clone();
                }
            }
        }
        lane
    }
}

#[test]
fn claude_root_resume_moves_the_lane_to_the_root_model() {
    let replies = (0..4).map(|i| text_reply(&format!("reply {i}"))).collect();
    let (port, seen) = mock_server(replies, Naming::Fail);
    let root = Root::new(port);
    let (code, stderr) = root.run(
        &["--model", "anthropic/claude-sonnet-4-5", "--thinking", "low"],
        "hello",
    );
    assert_eq!(code, Some(0), "{stderr}");
    // A plain `--model` on resume keeps the stored lane (print-continue-keeps-lane-model).
    let (code, stderr) = root.run(&["-c", "--model", "claude-subscription/claude-opus-5-5"], "plain");
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(
        root.lane()["model"],
        json!({"provider": "anthropic", "modelId": "claude-sonnet-4-5"})
    );
    // `--claude -c`: the lane moves to the root model; its thinking level stays.
    let (code, stderr) = root.run(&["--claude", "-c"], "rooted");
    assert_eq!(code, Some(0), "{stderr}");
    let lane = root.lane();
    assert_eq!(
        (&lane["model"], &lane["thinkingLevel"]),
        (
            &json!({"provider": "claude-subscription", "modelId": "claude-opus-5-5"}),
            &json!("low")
        )
    );
    // Already on the root's provider: `--claude --model sonnet -c` keeps the lane's own Claude model.
    let (code, stderr) = root.run(&["--claude", "--model", "sonnet", "-c"], "again");
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(root.lane()["model"]["modelId"], "claude-opus-5-5");
    let requests = seen.lock().unwrap().requests.clone();
    let models: Vec<&str> = requests.iter().map(|r| r["body"]["model"].as_str().unwrap()).collect();
    assert_eq!(
        models,
        [
            "claude-sonnet-4-5",
            "claude-sonnet-4-5",
            "claude-opus-5-5[1m]",
            "claude-opus-5-5[1m]"
        ]
    );
}
