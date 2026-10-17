//! The root lane validates and coerces the model's `rlm` arguments like the TypeScript harness's `prepareToolCall`
//! (`validateToolArguments`): a call without `code` fails with TypeScript's validation text (the expectation below is
//! recorded from TypeScript in crates/agent/fixtures/validation.json, case `rlm {}`), and `code: 5` runs as the
//! string "5".

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool_results};
use serde_json::{Value, json};

fn rlm_call(id: &str, input: &Value) -> String {
    let start = json!({"type": "message_start", "message": {"id": "msg_t", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}});
    [
        start,
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": "rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

fn texts(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect(),
        other => panic!("tool result content {other}"),
    }
}

#[test]
fn session_core_rlm_arguments_are_validated_and_coerced_like_typescript() {
    let session = actor(
        in_order(vec![
            rlm_call("t1", &json!({})),
            rlm_call("t2", &json!({"code": 6 * 7})),
            text("done"),
        ]),
        Setup::default(),
    );
    let ended = session.prompt("go");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let requests = session.requests();
    assert_eq!(requests.len(), 3);
    let invalid = texts(&tool_results(&requests[1])[0]);
    assert_eq!(
        invalid,
        "<harness>\n[error] Validation failed for tool \"rlm\":\n  - code: must have required properties code\n\nReceived arguments:\n{}\n</harness>"
    );
    let coerced = texts(&tool_results(&requests[2])[1]);
    assert!(coerced.starts_with("42"), "{coerced}");
    drop(session);
}
