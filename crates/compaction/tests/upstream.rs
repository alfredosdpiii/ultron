//! Ported from packages/coding-agent/test/compaction-serialization.test.ts, with literal expectations.

use compaction::serialize_conversation;
use serde_json::json;

fn tool_result(text: &str) -> serde_json::Value {
    json!({ "role": "toolResult", "toolCallId": "tc1", "toolName": "read", "content": [{ "type": "text", "text": text }], "isError": false, "timestamp": 1 })
}

#[test]
fn compaction_serialize_truncates_long_tool_results() {
    let result = serialize_conversation(&[tool_result(&"x".repeat(5000))]);
    assert_eq!(
        result,
        format!(
            "[Tool result]: {}\n\n[... 3000 more characters truncated]",
            "x".repeat(2000)
        )
    );
}

#[test]
fn compaction_serialize_keeps_short_tool_results() {
    let short = "x".repeat(1500);
    assert_eq!(
        serialize_conversation(&[tool_result(&short)]),
        format!("[Tool result]: {short}")
    );
}

#[test]
fn compaction_serialize_never_truncates_user_or_assistant_text() {
    let long = "y".repeat(5000);
    let messages = [
        json!({ "role": "user", "content": [{ "type": "text", "text": long }], "timestamp": 1 }),
        json!({ "role": "assistant", "content": [{ "type": "text", "text": long }], "stopReason": "stop", "timestamp": 1 }),
    ];
    assert_eq!(
        serialize_conversation(&messages),
        format!("[User]: {long}\n\n[Assistant]: {long}")
    );
}

/// Lengths count UTF-16 units like JavaScript; a cut inside a surrogate pair drops the half pair instead of
/// keeping a lone surrogate (Rust strings cannot hold one).
#[test]
fn compaction_serialize_truncation_counts_utf16_and_keeps_pairs_whole() {
    let text = format!("{}😀tail", "a".repeat(1999));
    assert_eq!(
        serialize_conversation(&[tool_result(&text)]),
        format!(
            "[Tool result]: {}\n\n[... 5 more characters truncated]",
            "a".repeat(1999)
        )
    );
}
