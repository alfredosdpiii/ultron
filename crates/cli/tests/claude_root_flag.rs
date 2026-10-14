//! `ultron --claude -p` end to end (crates/claude-root `subscription_root`): the root lane runs on the
//! claude-subscription provider. The Rust binary, daemon and worker, a FAKE `claude`
//! (crates/ai/fixtures/claude-subscription/claude.py; PATH is only its directory plus /usr/bin:/bin, never the real
//! CLI) and a fake Messages upstream on loopback: the request carries the model `--claude` selected and exactly the
//! `rlm` tool.

mod common;
mod executable_support;

use common::{Naming, Sandbox, assert_private_home, mock_server};
use serde_json::{Value, json};
use std::path::Path;

fn text_reply(text: &str) -> String {
    let usage = json!({"input_tokens": 20, "output_tokens": 1});
    [
        json!({"type": "message_start", "message": {"id": "msg_flag", "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [], "stop_reason": null, "usage": usage}}),
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

/// Run `ultron <flags> -p hello` against a fresh fake upstream; the one upstream request body and stdout.
fn run(flags: &[&str], extra_env: &[(&str, &str)]) -> (Value, String) {
    let (port, seen) = mock_server(vec![text_reply("hi from claude")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    assert_private_home(&sandbox.root);
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"sessionNaming":{"enabled":false}}"#,
    )
    .unwrap();
    std::fs::write(
        sandbox.agent_dir.join("models.json"),
        format!(r#"{{"providers":{{"claude-subscription":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
    )
    .unwrap();
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ai/fixtures/claude-subscription/claude.py");
    let wrapper = sandbox.root.join("fake-bin/claude");
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    // Executed by path (ULTRON_CLAUDE_COMMAND), so written without a write descriptor in this process.
    executable_support::install(
        &wrapper,
        &format!("#!/bin/sh\nexec {python} -I {} \"$@\"\n", fixture.display()),
    );
    let tmp = sandbox.root.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut command = sandbox.ultron();
    command
        .env_remove("ANTHROPIC_API_KEY")
        .env("PATH", format!("{}:/usr/bin:/bin", wrapper.parent().unwrap().display()))
        .env("ULTRON_CLAUDE_COMMAND", &wrapper)
        .env("FAKE_LOG", sandbox.root.join("claude-log.jsonl"))
        .env("TMPDIR", &tmp)
        .args(flags)
        .args(["-p", "hello"]);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 1, "one model call: {stdout}{stderr}");
    (requests[0]["body"].clone(), stdout)
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn claude_root_claude_flag_runs_the_root_on_the_claude_subscription() {
    let (body, stdout) = run(&["--claude"], &[]);
    assert_eq!(stdout, "hi from claude\n");
    assert_eq!(body["model"], "claude-opus-5-5[1m]", "the default root model");
    assert_eq!(tool_names(&body), ["mcp__ultron__rlm"], "the model's tool list");
    // --model and ULTRON_ROOT=claude: the same selection rules as TypeScript.
    let (body, _) = run(&["--model", "sonnet"], &[("ULTRON_ROOT", "claude")]);
    assert_eq!(body["model"], "claude-sonnet-5-5[1m]");
    assert_eq!(tool_names(&body), ["mcp__ultron__rlm"]);
}

#[test]
fn claude_root_claude_flag_refuses_another_providers_model() {
    let sandbox = Sandbox::new(1);
    let output = sandbox
        .ultron()
        .env("PATH", "/usr/bin:/bin")
        .args(["--claude", "--model", "openai/gpt-5", "-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Error: --claude runs the root agent on Claude Code, which does not serve openai/gpt-5. Use --model claude-code/<model> (for example claude-code/sonnet), or leave out --claude.\n"
    );
}
