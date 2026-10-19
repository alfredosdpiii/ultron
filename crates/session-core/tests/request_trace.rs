//! Request traces (`ULTRON_REQUEST_TRACE`, crate::request_trace; Rust-only feature, no TypeScript reference): the
//! session actor against a scripted provider on loopback writes one JSON line per answered root request with the
//! delta from the previous request of its lane: the first request starts the lane, the next turn appends (the
//! answer and the new prompt), and a compaction resets it with the reason `compaction`.

mod p_session_support;

use p_session_support::{Setup, actor, text};
use serde_json::Value;
use session_core::core::Op;

fn is_summary(body: &Value) -> bool {
    body["system"].to_string().contains("summarization")
}

fn lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn user_texts(messages: &Value) -> Vec<String> {
    messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| match &m["content"] {
            Value::String(text) => text.clone(),
            blocks => blocks[0]["text"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

#[test]
fn session_core_request_trace_appends_deltas_and_marks_the_compaction_reset() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("calls.jsonl");
    let script = Box::new(|body: &Value, index: usize| {
        if is_summary(body) {
            text("## Goal\nSummary from the model.")
        } else {
            text(&format!("Answer {index}."))
        }
    });
    let a = actor(
        script,
        Setup {
            vars: vec![
                ("ULTRON_REQUEST_TRACE", trace.display().to_string()),
                // Clock lines open a turn after a gap; off, the deltas are exact.
                ("ULTRON_CLOCK_CONTEXT", "off".into()),
            ],
            ..Setup::default()
        },
    );
    assert_eq!(a.prompt("first")["error"], Value::Null);
    assert_eq!(a.prompt("second")["error"], Value::Null);
    a.call(Op::Compact { instructions: None }).unwrap();
    assert_eq!(a.prompt("third")["error"], Value::Null);

    let records = lines(&trace);
    // The summary request is not a conversation request: three lines, all of the root lane.
    assert_eq!(records.len(), 3, "{records:#?}");
    let session = records[0]["session"].as_str().unwrap().to_string();
    for (i, record) in records.iter().enumerate() {
        assert_eq!(record["lane"], "main");
        assert_eq!(record["session"], session.as_str());
        assert_eq!(record["request"], i as u64 + 1);
        assert_eq!(record["model"], "anthropic/claude-sonnet-4-5");
        assert_eq!(record["stopReason"], "stop");
        assert_eq!(record["usage"]["output"], 5);
    }
    assert_eq!(records[0]["kind"], "start");
    assert_eq!(records[0]["base"], 0);
    assert_eq!(user_texts(&records[0]["messages"]), ["first"]);
    // The second request appends the first answer and the second prompt, nothing else.
    assert_eq!(records[1]["kind"], "append");
    let base = records[0]["messages"].as_array().unwrap().len();
    assert_eq!(records[1]["base"], base as u64);
    let appended = records[1]["messages"].as_array().unwrap();
    assert_eq!(appended.len(), 2, "{appended:#?}");
    assert_eq!(appended[0]["role"], "assistant");
    assert_eq!(appended[0]["content"][0]["text"], "Answer 0.");
    assert_eq!(user_texts(&records[1]["messages"]), ["second"]);
    // After the compaction the prefix changed: a reset with its reason and the whole context.
    assert_eq!(records[2]["kind"], "reset");
    assert_eq!(records[2]["reason"], "compaction");
    assert!(records[2]["at"].as_u64().unwrap() >= 1);
    let texts = user_texts(&records[2]["messages"]);
    assert!(
        texts[0].starts_with("The conversation history before this point was compacted"),
        "{texts:?}"
    );
    assert_eq!(texts.last().unwrap(), "third");

    let report = session_core::request_trace::check(&std::fs::read_to_string(&trace).unwrap()).unwrap();
    assert_eq!(report.requests, 3);
    assert_eq!(report.lanes, 1);
    assert_eq!(report.resets.len(), 1);
    assert_eq!(report.resets[0].reason, "compaction");
    assert_eq!(report.resets[0].line, 3);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&trace).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
