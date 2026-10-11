//! Rust writes, TypeScript reads: sessions written by the Rust `ultron -p` are found and continued by the
//! TypeScript product (`node packages/coding-agent/src/cli.ts`), which sends their history to the model, keeps
//! their name and appends its own run; the Rust build then continues the TypeScript-extended file. Private
//! ULTRON_CODING_AGENT_DIR / ULTRON_SERVER_DIR dirs; a scripted provider. Needs `node` on PATH.

mod common;

use common::{Naming, Sandbox, anthropic_text, mock_server, session_files};
use serde_json::{Value, json};
use std::process::Output;

const MODEL: &str = "anthropic/claude-sonnet-4-5";

fn tool_reply(id: &str, code: &str) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_2", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": "rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"code": code}).to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn typescript(sandbox: &Sandbox, args: &[&str]) -> Output {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    std::process::Command::new("node")
        .arg(root.join("packages/coding-agent/src/cli.ts"))
        .args(args)
        .env_clear()
        .current_dir(&sandbox.cwd)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &sandbox.root)
        .env("ULTRON_CODING_AGENT_DIR", &sandbox.agent_dir)
        .env("ULTRON_SERVER_DIR", sandbox.root.join("server"))
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("NO_COLOR", "1")
        .env("TZ", "UTC")
        .output()
        .unwrap()
}

fn texts(messages: &Value) -> Vec<String> {
    messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            m["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .map(|p| match p["type"].as_str() {
                            Some("text") => p["text"].as_str().unwrap_or("").to_string(),
                            Some("tool_use") => format!("tool_use:{}", p["input"]["code"].as_str().unwrap_or("")),
                            Some("tool_result") => format!("tool_result:{}", p["content"]),
                            other => format!("{other:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .unwrap_or_default()
        })
        .filter(|t| !t.starts_with("[clock]"))
        .collect()
}

fn records(text: &str) -> Vec<Value> {
    text.lines()
        .skip(1)
        .flat_map(|l| match serde_json::from_str::<Value>(l).unwrap() {
            Value::Array(items) => items,
            one => vec![one],
        })
        .collect()
}

#[test]
fn cli_cross_read_typescript_continues_a_rust_session_and_rust_continues_it_back() {
    if !node_available() {
        eprintln!("skipped: node is not on PATH");
        return;
    }
    let replies = vec![
        tool_reply("toolu_r1", "v = 20 + 1\nv"),
        anthropic_text("rust answer"),
        anthropic_text("ts answer"),
        anthropic_text("rust again"),
    ];
    let (port, seen) = mock_server(replies, Naming::Fail);
    let sandbox = Sandbox::new(port);
    let rust = sandbox
        .ultron()
        .args(["-p", "-n", "Rust made", "--model", MODEL, "compute"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&rust.stdout),
        "rust answer\n",
        "{}",
        String::from_utf8_lossy(&rust.stderr)
    );

    // TypeScript lists it for this cwd and continues it.
    let ts = typescript(&sandbox, &["-p", "--continue", "--model", MODEL, "from typescript"]);
    assert_eq!(
        String::from_utf8_lossy(&ts.stdout),
        "ts answer\n",
        "{}",
        String::from_utf8_lossy(&ts.stderr)
    );
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(
        texts(&requests[2]["body"]["messages"]),
        [
            "compute",
            "tool_use:v = 20 + 1\nv",
            "tool_result:\"21\"",
            "rust answer",
            "from typescript"
        ],
        "TypeScript sends the Rust session's history"
    );
    let files = session_files(&sandbox.sessions_dir());
    assert_eq!(files.len(), 1, "TypeScript appended to the Rust file: {files:?}");
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let all = records(&text);
    let results: Vec<&Value> = all.iter().filter(|r| r["namespace"] == "pi.result").collect();
    assert_eq!(results.len(), 2, "one terminal result per run");
    assert!(results.iter().all(|r| r["value"]["status"] == "completed"));
    let name = all.iter().rev().find(|r| r["namespace"] == "pi.session.name").unwrap();
    assert_eq!(name["value"], "Rust made", "TypeScript keeps the Rust session's name");

    // Rust continues the file TypeScript extended.
    let back = sandbox
        .ultron()
        .args(["-p", "-c", "--model", MODEL, "back to rust"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&back.stdout),
        "rust again\n",
        "{}",
        String::from_utf8_lossy(&back.stderr)
    );
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(
        texts(&requests[3]["body"]["messages"])[3..],
        ["rust answer", "from typescript", "ts answer", "back to rust"]
    );
}

#[test]
fn cli_cross_read_typescript_resumes_a_rust_fork_by_id() {
    if !node_available() {
        eprintln!("skipped: node is not on PATH");
        return;
    }
    let replies = vec![anthropic_text("one"), anthropic_text("forked"), anthropic_text("ts")];
    let (port, seen) = mock_server(replies, Naming::Fail);
    let sandbox = Sandbox::new(port);
    let first = sandbox
        .ultron()
        .args(["--mode", "json", "--model", MODEL, "hello"])
        .output()
        .unwrap();
    let source: Value = serde_json::from_slice(&first.stdout).unwrap();
    let fork = sandbox
        .ultron()
        .args([
            "--mode",
            "json",
            "--fork",
            source["sessionId"].as_str().unwrap(),
            "--model",
            MODEL,
            "in the fork",
        ])
        .output()
        .unwrap();
    let forked: Value = serde_json::from_slice(&fork.stdout).unwrap();
    let ts = typescript(
        &sandbox,
        &[
            "-p",
            "--session",
            forked["sessionId"].as_str().unwrap(),
            "--model",
            MODEL,
            "ts turn",
        ],
    );
    assert_eq!(
        String::from_utf8_lossy(&ts.stdout),
        "ts\n",
        "{}",
        String::from_utf8_lossy(&ts.stderr)
    );
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(
        texts(&requests[2]["body"]["messages"]),
        ["hello", "one", "in the fork", "forked", "ts turn"]
    );
    assert_eq!(session_files(&sandbox.sessions_dir()).len(), 2);
}
