//! Ported expectations of the TypeScript RPC tests: `rpc-jsonl.test.ts` (framing), the `RpcEventTranslator` case
//! of `ultron-native-rpc.test.ts`, `ultron-clock-context.test.ts` (clock lines), and `rpc-client-process-exit`,
//! `rpc-client-clear-queue` / `rpc-client-clone` (client).

use cli::lane::{format_clock_utc, format_gap, with_clock_lines};
use cli::rpc::client::{RpcClient, RpcClientOptions};
use cli::rpc::events::{LaneEvent, RpcEventTranslator, frame_to_assistant_message_event, queue_update};
use cli::rpc::jsonl::{JsonlLines, serialize_json_line};
use serde_json::{Value, json};

#[test]
fn cli_rpc_jsonl_keeps_unicode_separators() {
    let line = serialize_json_line(&json!({"text": "a\u{2028}b\u{2029}c"}));
    assert!(line.contains("a\u{2028}b\u{2029}c"));
    assert!(line.ends_with('\n'));
    let lines: Vec<String> = JsonlLines::new(line.as_bytes()).map(Result::unwrap).collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        serde_json::from_str::<Value>(&lines[0]).unwrap(),
        json!({"text": "a\u{2028}b\u{2029}c"})
    );
}

#[test]
fn cli_rpc_jsonl_handles_crlf_and_final_line() {
    let lines: Vec<String> = JsonlLines::new(&b"{\"a\":1}\r\n{\"b\":2}\r\n"[..])
        .map(Result::unwrap)
        .collect();
    assert_eq!(lines, ["{\"a\":1}", "{\"b\":2}"]);
    let lines: Vec<String> = JsonlLines::new(&b"{\"a\":1}"[..]).map(Result::unwrap).collect();
    assert_eq!(lines, ["{\"a\":1}"]);
    // Records are written like JSON.stringify: integral floats without a fraction.
    assert_eq!(
        serialize_json_line(&json!({"n": 3.0, "s": "x"})),
        "{\"n\":3,\"s\":\"x\"}\n"
    );
}

fn assistant() -> Value {
    json!({
        "role": "assistant", "content": [{"type": "text", "text": "ok"}], "api": "openai-completions",
        "provider": "mock", "model": "mock",
        "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        "stopReason": "stop", "timestamp": 2,
    })
}

#[test]
fn cli_rpc_translator_reconstructs_turn_and_agent_boundaries() {
    let mut translator = RpcEventTranslator::default();
    let user = json!({"role": "user", "content": "hi", "timestamp": 1});
    let events = [
        LaneEvent::RunStart,
        LaneEvent::MessageStart(user.clone()),
        LaneEvent::MessageEnd {
            message: user.clone(),
            in_run: true,
        },
        LaneEvent::MessageStart(assistant()),
        LaneEvent::MessageUpdate {
            usage: assistant()["usage"].clone(),
            event: json!({"type": "text_delta", "contentIndex": 0, "delta": "ok", "partial": assistant()}),
        },
        LaneEvent::MessageEnd {
            message: assistant(),
            in_run: true,
        },
        LaneEvent::RunEnd {
            error: None,
            ended_at: 1,
        },
    ];
    let lines: Vec<Value> = events.into_iter().flat_map(|e| translator.translate(e)).collect();
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "agent_start",
            "message_start",
            "message_end",
            "turn_start",
            "message_start",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
            "agent_settled",
        ]
    );
    assert_eq!(
        lines[5]["assistantMessageEvent"],
        json!({"type": "text_delta", "contentIndex": 0, "delta": "ok"})
    );
    assert_eq!(
        lines[7],
        json!({"type": "turn_end", "message": assistant(), "toolResults": []})
    );
    assert_eq!(lines[8]["messages"], json!([user, assistant()]));
    assert_eq!(lines[8]["willRetry"], false);
}

#[test]
fn cli_rpc_translator_reports_a_failed_run_as_an_error_message() {
    let mut translator = RpcEventTranslator::default();
    translator.translate(LaneEvent::RunStart);
    let lines = translator.translate(LaneEvent::RunEnd {
        error: Some("No API key".into()),
        ended_at: 7,
    });
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["message_start", "message_end", "agent_end", "agent_settled"]);
    assert_eq!(lines[0]["message"]["errorMessage"], "No API key");
    assert_eq!(lines[0]["message"]["provider"], "ultron");
    assert_eq!(lines[0]["message"]["timestamp"], 7);
    assert_eq!(lines[2]["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn cli_rpc_translator_tool_and_queue_events() {
    let mut translator = RpcEventTranslator::default();
    translator.translate(LaneEvent::ToolStart {
        id: "t1".into(),
        name: "rlm".into(),
        args: json!({"code": "1"}),
    });
    let update = translator.translate(LaneEvent::ToolUpdate {
        id: "t1".into(),
        name: "rlm".into(),
        partial: json!({"content": []}),
    });
    assert_eq!(
        update[0],
        json!({"type": "tool_execution_update", "toolCallId": "t1", "toolName": "rlm", "args": {"code": "1"}, "partialResult": {"content": []}})
    );
    translator.translate(LaneEvent::ToolEnd {
        id: "t1".into(),
        name: "rlm".into(),
        result: json!({}),
        is_error: false,
    });
    // After the end, the arguments are forgotten.
    let late = translator.translate(LaneEvent::ToolUpdate {
        id: "t1".into(),
        name: "rlm".into(),
        partial: json!(null),
    });
    assert!(late[0].get("args").is_none());
    assert_eq!(
        queue_update(&[
            ("steer".into(), "a".into()),
            ("followUp".into(), "b".into()),
            ("nextRun".into(), "c".into())
        ]),
        json!({"type": "queue_update", "steering": ["a"], "followUp": ["b", "c"]})
    );
    let end = translator.translate(LaneEvent::CompactionEnd {
        reason: "manual".into(),
        aborted: false,
        error: Some("boom".into()),
    });
    assert_eq!(
        end[0],
        json!({"type": "compaction_end", "reason": "manual", "aborted": false, "willRetry": false, "errorMessage": "boom"})
    );
}

#[test]
fn cli_rpc_frames_drop_partials() {
    let partial = json!({"content": [{"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {}}]});
    assert_eq!(
        frame_to_assistant_message_event(&json!({"type": "toolcall_start", "contentIndex": 0, "partial": partial})),
        Some(json!({"type": "toolcall_start", "contentIndex": 0, "id": "c1", "toolName": "rlm"}))
    );
    assert_eq!(
        frame_to_assistant_message_event(
            &json!({"type": "toolcall_end", "contentIndex": 0, "toolCall": {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "x"}}, "partial": partial})
        ),
        Some(
            json!({"type": "toolcall_end", "contentIndex": 0, "toolCall": {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "x"}}})
        )
    );
    assert_eq!(
        frame_to_assistant_message_event(
            &json!({"type": "toolcall_delta", "contentIndex": 0, "delta": "", "partial": partial})
        ),
        None
    );
    assert_eq!(
        frame_to_assistant_message_event(
            &json!({"type": "text_end", "contentIndex": 1, "content": "hi", "partial": {}})
        ),
        Some(json!({"type": "text_end", "contentIndex": 1, "content": "hi"}))
    );
    assert_eq!(
        frame_to_assistant_message_event(&json!({"type": "start", "partial": {}})),
        Some(json!({"type": "start"}))
    );
    assert_eq!(
        frame_to_assistant_message_event(&json!({"type": "done", "reason": "stop"})),
        None
    );
}

/// `Date.UTC(2026, 9, 5, 12, 0)`.
const T0: i64 = 1_791_201_600_000;

fn user(text: &str, timestamp: i64) -> Value {
    json!({"role": "user", "content": text, "timestamp": timestamp})
}

fn answer(timestamp: i64) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}], "timestamp": timestamp})
}

#[test]
fn cli_rpc_clock_lines_open_conversations_and_follow_gaps() {
    let messages = [
        user("a", T0),
        answer(T0 + 1000),
        user("b", T0 + 5 * 60_000),
        answer(T0 + 6 * 60_000),
        user("c", T0 + 3 * 3_600_000),
    ];
    let out = with_clock_lines(&messages).unwrap();
    let text = |m: &Value| m["content"][0]["text"].as_str().map(str::to_string);
    let lines: Vec<String> = out
        .iter()
        .filter_map(text)
        .filter(|t| t.starts_with("[clock] "))
        .collect();
    assert_eq!(
        lines,
        [
            "[clock] 2026-10-05 12:00 UTC+00:00".to_string(),
            "[clock] 2026-10-05 15:00 UTC+00:00, 2 h after the previous message".to_string(),
        ]
    );
    assert_eq!(out.len(), 7);
    assert_eq!(out[6], messages[4]);
    assert_eq!(out[0]["timestamp"], T0);
    assert_eq!(with_clock_lines(&messages), Some(out));
    assert_eq!(with_clock_lines(&[user("x", 0)]), None);
    // Exactly the gap counts; one millisecond short does not.
    assert!(with_clock_lines(&[answer(T0), user("y", T0 + 600_000)]).is_some());
    assert!(with_clock_lines(&[answer(T0), user("y", T0 + 599_999)]).is_none());
}

#[test]
fn cli_rpc_clock_formats() {
    assert_eq!(format_clock_utc(T0), "2026-10-05 12:00 UTC+00:00");
    assert_eq!(format_gap(12 * 60_000), "12 min");
    assert_eq!(format_gap(119 * 60_000), "119 min");
    assert_eq!(format_gap(3 * 3_600_000), "3 h");
    assert_eq!(format_gap(47 * 3_600_000), "47 h");
    assert_eq!(format_gap(50 * 3_600_000), "2 d");
}

fn script_client(script: &str) -> (tempfile::TempDir, RpcClient) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.sh");
    std::fs::write(&path, script).unwrap();
    let client = RpcClient::start(&RpcClientOptions {
        program: "/bin/sh".into(),
        program_args: vec![path.display().to_string()],
        ..Default::default()
    })
    .unwrap();
    (dir, client)
}

#[test]
fn cli_rpc_client_rejects_a_request_when_the_process_exits() {
    let (_dir, mut client) = script_client("read line\nexit 43\n");
    let error = client.get_commands().unwrap_err();
    assert!(
        error.starts_with("Agent process exited (code=43 signal=null)"),
        "{error}"
    );
}

#[test]
fn cli_rpc_client_sends_clear_queue_and_clone() {
    // An agent that answers each command with a canned response carrying its id.
    let (_dir, mut client) = script_client(
        r#"while read line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *clear_queue*) printf '{"id":"%s","type":"response","command":"clear_queue","success":true,"data":{"steering":["Change direction"],"followUp":["Summarize when finished"]}}\n' "$id" ;;
    *clone*) printf '{"id":"%s","type":"response","command":"clone","success":true,"data":{"cancelled":false}}\n' "$id" ;;
    *) printf '{"id":"%s","type":"response","command":"x","success":false,"error":"nope"}\n' "$id" ;;
  esac
done
"#,
    );
    assert_eq!(
        client.clear_queue().unwrap(),
        json!({"steering": ["Change direction"], "followUp": ["Summarize when finished"]})
    );
    assert_eq!(client.clone_session().unwrap(), json!({"cancelled": false}));
    assert_eq!(client.get_state().unwrap_err(), "nope");
    let sent: Vec<&Value> = client.transcript.iter().collect();
    assert_eq!(sent[0]["id"], "req_1");
    assert_eq!(sent[1]["id"], "req_2");
    assert_eq!(client.stop(), Some(0));
}

#[test]
fn cli_rpc_watch_events_decode_as_lane_events() {
    use cli::rpc::events::LaneEvent;
    // A message outside a run (a `bash` result) is not part of the next run's messages.
    let outside = json!({"type": "message_end", "lane": "main", "message": {"role": "bashExecution"}});
    let inside = json!({"type": "message_end", "lane": "main", "runId": "r1", "message": {"role": "user"}});
    assert_eq!(
        LaneEvent::from_watch(&outside),
        Some(LaneEvent::MessageEnd {
            message: json!({"role": "bashExecution"}),
            in_run: false
        })
    );
    assert!(matches!(
        LaneEvent::from_watch(&inside),
        Some(LaneEvent::MessageEnd { in_run: true, .. })
    ));
    // Only a failed run carries its error; an aborted one ends without.
    let failed = json!({"type": "run_end", "status": "failed", "error": {"message": "boom"}, "endedAt": 5});
    let aborted = json!({"type": "run_end", "status": "aborted", "endedAt": 6});
    assert_eq!(
        LaneEvent::from_watch(&failed),
        Some(LaneEvent::RunEnd {
            error: Some("boom".into()),
            ended_at: 5
        })
    );
    assert_eq!(
        LaneEvent::from_watch(&aborted),
        Some(LaneEvent::RunEnd {
            error: None,
            ended_at: 6
        })
    );
    // A tool call's start frame names the call; queue items show their text; the name event is not RPC's.
    let start = json!({"type": "message_update", "lane": "main", "runId": "r1", "message": {"role": "assistant", "usage": {}},
        "frame": {"type": "toolcall_start", "contentIndex": 0, "id": "t1", "toolName": "rlm"}});
    let Some(LaneEvent::MessageUpdate { event, .. }) = LaneEvent::from_watch(&start) else {
        panic!("a message update")
    };
    assert_eq!(
        frame_to_assistant_message_event(&event),
        Some(json!({"type": "toolcall_start", "contentIndex": 0, "id": "t1", "toolName": "rlm"}))
    );
    let queued = json!({"type": "queue_update", "lane": "main", "queues": [
        {"type": "message", "kind": "followUp", "entryId": "e1", "message": {"role": "user", "content": [{"type": "text", "text": "later"}]}}
    ]});
    assert_eq!(
        LaneEvent::from_watch(&queued),
        Some(LaneEvent::QueueUpdate(vec![("followUp".into(), "later".into())]))
    );
    assert_eq!(
        LaneEvent::from_watch(&json!({"type": "session_name", "name": "x"})),
        None
    );
}
