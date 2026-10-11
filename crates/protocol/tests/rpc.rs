//! Replays `fixtures/rpc.json`, recorded from the TypeScript RPC mode by `fixtures/generate-rpc.mjs`:
//! every command/response line parses into the Rust types and re-serializes byte-identically, and the
//! LF-only JSONL reader splits the same chunks into the same lines.

use protocol::rpc::{
    Command, ExtensionUiAnswer, ExtensionUiResponse, JsonlLineReader, RpcCommand, RpcResponse, serialize_json_line,
};
use serde_json::Value as Json;

fn fixture() -> Json {
    serde_json::from_str(include_str!("../fixtures/rpc.json")).expect("fixture")
}

fn lines(fixture: &Json, key: &str) -> Vec<String> {
    fixture[key]
        .as_array()
        .expect(key)
        .iter()
        .map(|l| l.as_str().expect("line").to_owned())
        .collect()
}

#[test]
fn protocol_rpc_commands_round_trip_typescript_lines() {
    let lines = lines(&fixture(), "commands");
    assert_eq!(lines.len(), 40);
    for line in &lines {
        let command: RpcCommand = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert_eq!(&serialize_json_line(&command).expect("serialize"), line);
    }
    let first: RpcCommand = serde_json::from_str(&lines[0]).expect("prompt");
    assert_eq!(first.id.as_deref(), Some("1"));
    assert!(matches!(first.command, Command::Prompt { ref message, .. } if message == "hi\u{2028}there"));
}

#[test]
fn protocol_rpc_rejects_unknown_command_types() {
    let error = serde_json::from_str::<RpcCommand>(r#"{"id":"1","type":"launch_rockets"}"#).expect_err("unknown");
    assert!(
        error.to_string().contains("unknown variant `launch_rockets`"),
        "{error}"
    );
}

#[test]
fn protocol_rpc_responses_round_trip_typescript_lines() {
    let fixture = fixture();
    for line in lines(&fixture, "responses") {
        let response: RpcResponse = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert_eq!(serialize_json_line(&response).expect("serialize"), line);
    }
    let error = RpcResponse::error(Some("y".into()), "nope", "Unknown command: nope");
    assert_eq!(
        serialize_json_line(&error).expect("serialize"),
        lines(&fixture, "responses")[3]
    );
    let ok = RpcResponse::success(Some("1".into()), "prompt", None);
    assert_eq!(
        serialize_json_line(&ok).expect("serialize"),
        lines(&fixture, "responses")[0]
    );
}

#[test]
fn protocol_rpc_extension_ui_responses_round_trip_typescript_lines() {
    let lines = lines(&fixture(), "uiResponses");
    let parsed: Vec<ExtensionUiResponse> = lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l}: {e}")))
        .collect();
    assert_eq!(parsed[0].answer, ExtensionUiAnswer::Value { value: "picked".into() });
    assert_eq!(parsed[1].answer, ExtensionUiAnswer::Confirmed { confirmed: true });
    assert_eq!(parsed[2].answer, ExtensionUiAnswer::Cancelled { cancelled: true });
    for (response, line) in parsed.iter().zip(&lines) {
        assert_eq!(&serialize_json_line(response).expect("serialize"), line);
    }
}

#[test]
fn protocol_rpc_jsonl_reader_splits_like_typescript() {
    let fixture = fixture();
    for script in fixture["reader"].as_array().expect("scripts") {
        let mut reader = JsonlLineReader::default();
        let mut got = Vec::new();
        for chunk in script["chunks"].as_array().expect("chunks") {
            let hex = chunk.as_str().expect("hex");
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                .collect();
            got.extend(reader.push(&bytes));
        }
        got.extend(reader.end());
        assert_eq!(Json::from(got), script["lines"], "{script}");
    }
}
