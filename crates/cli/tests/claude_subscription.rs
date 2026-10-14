//! `ultron -p --model claude-subscription/sonnet` end to end: the Rust binary, the session daemon and worker, the
//! real Python kernel, a FAKE `claude` (crates/ai/fixtures/claude-subscription/claude.py; never the real CLI) that
//! spawns the worker's own `ultron __internal inert-mcp` inventory server, and a fake Messages upstream on loopback
//! (models.json `providers.claude-subscription.baseUrl`). The model calls `rlm`, the Rust host runs the cell, the
//! result is replayed natively on the next call, and the advertised inventory is exactly `[rlm]` everywhere.

mod common;
mod executable_support;

use common::{Naming, Sandbox, assert_private_home, mock_server};
use serde_json::{Value, json};
use std::path::Path;

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn tool_reply() -> String {
    let usage =
        json!({"input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0});
    sse(&[
        json!({"type": "message_start", "message": {"id": "msg_e2e_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5", "content": [], "stop_reason": null, "usage": usage}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "compute it in the REPL"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-e2e"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_e2e", "name": "mcp__ultron__rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": json!({"code": "print(6 * 7)"}).to_string()}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}}),
        json!({"type": "message_stop"}),
    ])
}

fn text_reply(text: &str) -> String {
    let usage = json!({"input_tokens": 20, "output_tokens": 1});
    sse(&[
        json!({"type": "message_start", "message": {"id": "msg_e2e_2", "type": "message", "role": "assistant", "model": "claude-sonnet-5-5", "content": [], "stop_reason": null, "usage": usage}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
        json!({"type": "message_stop"}),
    ])
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
fn cli_claude_subscription_turn_runs_rlm_in_the_host_and_replays_natively() {
    let (port, seen) = mock_server(vec![tool_reply(), text_reply("forty-two seen")], Naming::Fail);
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
    let log = sandbox.root.join("claude-log.jsonl");
    let tmp = sandbox.root.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let output = sandbox
        .ultron()
        .env_remove("ANTHROPIC_API_KEY")
        // Only the fake and the system tools on PATH: even if ULTRON_CLAUDE_COMMAND were ignored, no real `claude`
        // (e.g. ~/.local/bin/claude) could be found.
        .env("PATH", format!("{}:/usr/bin:/bin", wrapper.parent().unwrap().display()))
        .env("ULTRON_CLAUDE_COMMAND", &wrapper)
        .env("FAKE_LOG", &log)
        .env("FAKE_REMINDER", "1")
        .env("TMPDIR", &tmp)
        .args([
            "-p",
            "--model",
            "claude-subscription/sonnet",
            "what is six times seven?",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "forty-two seen\n", "{stderr}");

    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 2, "one upstream request per model call");
    for request in &requests {
        assert_eq!(request["url"], "/v1/messages?beta=true");
        assert_eq!(
            tool_names(&request["body"]),
            ["mcp__ultron__rlm"],
            "the model's tool list"
        );
        assert_eq!(request["body"]["model"], "claude-sonnet-5-5[1m]");
    }
    // The second call replays the first turn natively: signed thinking verbatim, the host's own cell result.
    let messages = requests[1]["body"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    // Ultron's clock line, then the prompt: the first turn exactly as Ultron sent it.
    assert!(
        messages[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("[clock] ")
    );
    assert_eq!(
        messages[0]["content"][1],
        json!({"type": "text", "text": "what is six times seven?"})
    );
    assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
    assert_eq!(
        messages[1]["content"],
        json!([
            {"type": "thinking", "thinking": "compute it in the REPL", "signature": "sig-e2e"},
            {"type": "tool_use", "id": "toolu_e2e", "name": "mcp__ultron__rlm", "input": {"code": "print(6 * 7)"}},
        ])
    );
    let result = &messages[2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_e2e");
    let text = match &result["content"] {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    assert!(text.contains("42"), "the Rust host ran the cell: {text}");
    // Native's per-request reminder was appended by the fake CLI and removed by the relay's restoration.
    assert!(!text.contains("userEmail"), "{text}");

    let records: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 2, "one claude process per model call");
    let ultron = std::fs::canonicalize(env!("CARGO_BIN_EXE_ultron")).unwrap();
    for record in &records {
        let mcp = &record["mcp"]["ultron"];
        let listed: Vec<&str> = mcp["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(listed, ["rlm"], "MCP tools/list");
        assert_eq!(mcp["call"]["isError"], true, "tools/call is always denied");
        assert_eq!(record["native_tools"], json!(["mcp__ultron__rlm"]));
        let argv: Vec<&str> = record["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let config: Value =
            serde_json::from_str(argv[argv.iter().position(|a| *a == "--mcp-config").unwrap() + 1]).unwrap();
        let server = &config["mcpServers"]["ultron"];
        assert_eq!(
            std::fs::canonicalize(server["command"].as_str().unwrap()).unwrap(),
            ultron
        );
        assert_eq!(
            server["args"].as_array().unwrap()[..2],
            [json!("__internal"), json!("inert-mcp")]
        );
        assert!(record["env"].get("ANTHROPIC_API_KEY").is_none());
        assert!(record["cwd"].as_str().unwrap().starts_with(tmp.to_str().unwrap()));
    }
    // The tool round's earlier user frame replayed without a query.
    let frames = records[1]["frames"].as_array().unwrap();
    assert_eq!(frames[0]["shouldQuery"], false);
    assert_eq!(frames[1]["type"], "assistant");
    assert!(frames[2].get("shouldQuery").is_none());
}
