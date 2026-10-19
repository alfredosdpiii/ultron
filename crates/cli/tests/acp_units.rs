//! `cli::acp::translate` and `cli::acp::changes` with literal inputs and expectations: prompt blocks to the user
//! message, the RPC event stream to ACP updates, turn ends to stop reasons, replays, and the per-cell file changes.

use cli::acp::changes::Tracker;
use cli::acp::translate::{
    Action, FileChanges, Translator, TurnEnd, replay, tool_call, tool_title, tool_update, turn_end, usage_update,
    user_message,
};
use serde_json::{Value, json};

#[test]
fn cli_acp_user_message_joins_blocks() {
    let message = user_message(&[
        json!({"type": "text", "text": "Look at "}),
        json!({"type": "resource_link", "uri": "file:///w/a.rs", "name": "a.rs"}),
        json!({"type": "text", "text": " please"}),
        json!({"type": "text", "text": "Second part"}),
        json!({"type": "resource", "resource": {"uri": "file:///w/b.rs", "text": "fn b() {}"}}),
        json!({"type": "image", "data": "AAAA", "mimeType": "image/png"}),
        json!({"type": "resource", "resource": {"uri": "file:///w/c.png", "blob": "BBBB", "mimeType": "image/png"}}),
        json!({"type": "resource", "resource": {"uri": "file:///w/d.bin", "blob": "CCCC"}}),
    ])
    .unwrap();
    assert_eq!(
        message.text,
        "Look at [@a.rs](file:///w/a.rs) please\n\nSecond part\n<context ref=\"file:///w/b.rs\">\nfn b() {}\n</context>\n[binary resource file:///w/d.bin omitted]"
    );
    assert_eq!(
        message.images,
        vec![
            json!({"type": "image", "data": "AAAA", "mimeType": "image/png"}),
            json!({"type": "image", "data": "BBBB", "mimeType": "image/png"}),
        ]
    );
    // A link without a name shows its uri.
    let message = user_message(&[json!({"type": "resource_link", "uri": "file:///x", "name": ""})]).unwrap();
    assert_eq!(message.text, "[@file:///x](file:///x)");
}

#[test]
fn cli_acp_user_message_rejects_empty_and_unknown() {
    assert_eq!(user_message(&[]), Err("The prompt has no content".into()));
    assert_eq!(
        user_message(&[json!({"type": "text", "text": "  "})]),
        Err("The prompt has no content".into())
    );
    assert_eq!(
        user_message(&[json!({"type": "video"})]),
        Err("Unsupported content block type: video".into())
    );
    assert_eq!(
        user_message(&[json!({"type": "image", "data": "x"})]),
        Err("An image content block needs data and mimeType".into())
    );
    // An image alone is a prompt.
    assert!(user_message(&[json!({"type": "image", "data": "x", "mimeType": "image/png"})]).is_ok());
}

#[test]
fn cli_acp_tool_titles() {
    assert_eq!(tool_title("rlm", &json!({"code": "\n\n  x = 1  \ny = 2"})), "`x = 1`");
    assert_eq!(tool_title("rlm", &json!({"code": "print(`a`)"})), "`print('a')`");
    assert_eq!(tool_title("rlm", &json!({"code": ""})), "rlm");
    assert_eq!(tool_title("rlm", &json!({})), "rlm");
    let long = "a".repeat(81);
    assert_eq!(
        tool_title("rlm", &json!({"code": long})),
        format!("`{}...`", "a".repeat(80))
    );
    let exact = "b".repeat(80);
    assert_eq!(
        tool_title("rlm", &json!({"code": exact})),
        format!("`{}`", "b".repeat(80))
    );
}

#[test]
fn cli_acp_events_become_updates() {
    let mut t = Translator::default();
    let delta =
        |kind: &str, d: &str| json!({"type": "message_update", "assistantMessageEvent": {"type": kind, "delta": d}});
    assert_eq!(
        t.translate(&delta("text_delta", "Hi")),
        vec![Action::Update(
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hi"}})
        )]
    );
    assert_eq!(
        t.translate(&delta("thinking_delta", "hm")),
        vec![Action::Update(
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}})
        )]
    );
    assert!(t.translate(&delta("text_delta", "")).is_empty());
    assert!(t.translate(&delta("toolcall_delta", "{")).is_empty());
    let args = json!({"code": "print(1)"});
    let start =
        t.translate(&json!({"type": "tool_execution_start", "toolCallId": "c1", "toolName": "rlm", "args": args}));
    assert_eq!(
        start,
        vec![
            Action::CellStarted,
            Action::Update(json!({
                "sessionUpdate": "tool_call", "toolCallId": "c1", "title": "`print(1)`", "kind": "execute",
                "status": "in_progress", "content": [{"type": "content", "content": {"type": "text", "text": "```python\nprint(1)\n```"}}],
                "rawInput": {"code": "print(1)"},
            })),
        ]
    );
    // A progress update keeps the code first; the partial output follows.
    let update = t.translate(
        &json!({"type": "tool_execution_update", "toolCallId": "c1", "toolName": "rlm",
        "partialResult": {"content": [{"type": "text", "text": "1\n"}]}}),
    );
    assert_eq!(
        update,
        vec![Action::Update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "in_progress",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "```python\nprint(1)\n```"}},
                {"type": "content", "content": {"type": "text", "text": "```\n1\n```"}},
            ],
        }))]
    );
    let end = t.translate(
        &json!({"type": "tool_execution_end", "toolCallId": "c1", "toolName": "rlm",
        "result": {"content": [{"type": "text", "text": "1"}]}, "isError": true}),
    );
    assert_eq!(
        end,
        vec![Action::CellEnded {
            id: "c1".into(),
            args: args.clone(),
            result: json!({"content": [{"type": "text", "text": "1"}]}),
            is_error: true
        }]
    );
    // The cell's arguments are dropped once it ended.
    let again = t.translate(&json!({"type": "tool_execution_end", "toolCallId": "c1", "result": {}}));
    assert_eq!(
        again,
        vec![Action::CellEnded {
            id: "c1".into(),
            args: Value::Null,
            result: json!({}),
            is_error: false
        }]
    );
    let messages = vec![json!({"role": "assistant"})];
    assert_eq!(
        t.translate(&json!({"type": "agent_end", "messages": messages})),
        vec![Action::RunEnded(messages)]
    );
    assert_eq!(
        t.translate(&json!({"type": "session_info_changed", "name": "Fix it"})),
        vec![Action::Update(
            json!({"sessionUpdate": "session_info_update", "title": "Fix it"})
        )]
    );
    let ui = json!({"type": "extension_ui_request", "id": "u1", "method": "confirm"});
    assert_eq!(t.translate(&ui), vec![Action::ExtensionUi(ui.clone())]);
    for ignored in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "queue_update",
        "entry_appended",
    ] {
        assert!(t.translate(&json!({"type": ignored})).is_empty(), "{ignored}");
    }
}

#[test]
fn cli_acp_cell_end_updates() {
    let args = json!({"code": "w()"});
    let output =
        json!({"content": [{"type": "text", "text": "done\n"}, {"type": "image"}, {"type": "text", "text": "ok"}]});
    let plain = tool_update("c", &args, &output, Some((false, &FileChanges::default())));
    assert_eq!(
        plain,
        json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "c", "status": "completed",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "```python\nw()\n```"}},
                {"type": "content", "content": {"type": "text", "text": "```\ndone\n\nok\n```"}},
            ],
            "rawOutput": {"output": "done\n\nok", "isError": false},
        })
    );
    let changes = FileChanges {
        changed: vec!["/w/a.txt".into(), "/w/b.txt".into()],
        deleted: vec!["/w/c.txt".into()],
        diffs: vec![json!({"type": "diff", "path": "/w/a.txt", "oldText": null, "newText": "a"})],
    };
    let edited = tool_update("c", &args, &json!({"content": []}), Some((true, &changes)));
    assert_eq!(
        edited,
        json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "c", "status": "failed", "kind": "edit",
            "locations": [{"path": "/w/a.txt"}, {"path": "/w/b.txt"}],
            "content": [
                {"type": "content", "content": {"type": "text", "text": "```python\nw()\n```"}},
                {"type": "diff", "path": "/w/a.txt", "oldText": null, "newText": "a"},
                {"type": "content", "content": {"type": "text", "text": "Deleted: /w/c.txt"}},
            ],
            "rawOutput": {"output": "", "isError": true},
        })
    );
    assert_eq!(
        tool_call("t", "other", &json!({"x": 1})),
        json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "other", "kind": "other",
            "status": "in_progress", "content": [], "rawInput": {"x": 1}})
    );
}

#[test]
fn cli_acp_turn_ends() {
    let answer = |reason: &str, error: Option<&str>| {
        let mut m = json!({"role": "assistant", "stopReason": reason});
        if let Some(error) = error {
            m["errorMessage"] = json!(error);
        }
        vec![json!({"role": "user"}), m, json!({"role": "toolResult"})]
    };
    assert_eq!(turn_end(&answer("stop", None), false), TurnEnd::Stop("end_turn"));
    assert_eq!(turn_end(&answer("toolUse", None), false), TurnEnd::Stop("end_turn"));
    assert_eq!(turn_end(&answer("stop", None), true), TurnEnd::Stop("cancelled"));
    assert_eq!(turn_end(&answer("aborted", None), false), TurnEnd::Stop("cancelled"));
    assert_eq!(turn_end(&answer("length", None), false), TurnEnd::Stop("max_tokens"));
    assert_eq!(
        turn_end(
            &answer("error", Some("The model refused to complete the request")),
            false
        ),
        TurnEnd::Stop("refusal")
    );
    assert_eq!(
        turn_end(&answer("error", Some("Provider stopped with: sensitive")), false),
        TurnEnd::Stop("refusal")
    );
    assert_eq!(
        turn_end(&answer("error", Some("401 bad key")), false),
        TurnEnd::Failed("401 bad key".into())
    );
    assert_eq!(
        turn_end(&answer("error", None), false),
        TurnEnd::Failed("The model request failed".into())
    );
    assert_eq!(turn_end(&answer("error", Some("x")), true), TurnEnd::Stop("cancelled"));
    assert_eq!(turn_end(&[], false), TurnEnd::Stop("end_turn"));
    // The newest answer decides.
    let mut both = answer("error", Some("old"));
    both.push(json!({"role": "assistant", "stopReason": "stop"}));
    assert_eq!(turn_end(&both, false), TurnEnd::Stop("end_turn"));
}

#[test]
fn cli_acp_usage_updates() {
    let messages = vec![
        json!({"role": "assistant", "usage": {"totalTokens": 5}}),
        json!({"role": "assistant", "usage": {"input": 10, "output": 5, "cacheRead": 2, "cacheWrite": 1, "totalTokens": 0,
            "cost": {"total": 0.25}}}),
    ];
    assert_eq!(
        usage_update(&messages, Some(200_000)),
        Some(
            json!({"sessionUpdate": "usage_update", "used": 18, "size": 200_000, "cost": {"amount": 0.25, "currency": "USD"}})
        )
    );
    assert_eq!(
        usage_update(&messages[..1], Some(100)),
        Some(json!({"sessionUpdate": "usage_update", "used": 5, "size": 100}))
    );
    assert_eq!(usage_update(&messages, None), None);
    assert_eq!(usage_update(&messages, Some(0)), None);
    assert_eq!(usage_update(&[], Some(10)), None);
}

#[test]
fn cli_acp_replay_of_a_conversation() {
    let messages = vec![
        json!({"role": "user", "content": "plain"}),
        json!({"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image", "data": "D", "mimeType": "image/png"}]}),
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "plan"}, {"type": "text", "text": ""},
            {"type": "text", "text": "Running"}, {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "1"}},
            {"type": "toolCall", "id": "c2", "name": "rlm", "arguments": {"code": "2"}},
        ]}),
        json!({"role": "toolResult", "toolCallId": "c1", "content": [{"type": "text", "text": "1"}], "isError": false}),
        json!({"role": "toolResult", "toolCallId": "zz", "content": []}),
        json!({"role": "custom", "content": "skip"}),
    ];
    let updates = replay(&messages);
    let kinds: Vec<&str> = updates.iter().filter_map(|u| u["sessionUpdate"].as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "user_message_chunk",
            "user_message_chunk",
            "user_message_chunk",
            "agent_thought_chunk",
            "agent_message_chunk",
            "tool_call",
            "tool_call",
            "tool_call_update",
            "tool_call_update"
        ]
    );
    assert_eq!(updates[0]["content"], json!({"type": "text", "text": "plain"}));
    assert_eq!(
        updates[2]["content"],
        json!({"type": "image", "data": "D", "mimeType": "image/png"})
    );
    assert_eq!(updates[3]["content"]["text"], "plan");
    assert_eq!(updates[7]["toolCallId"], "c1");
    assert_eq!(updates[7]["status"], "completed");
    assert_eq!(updates[7]["content"][1]["content"]["text"], "```\n1\n```");
    // A cell with no result is reported failed at the end.
    assert_eq!(updates[8]["toolCallId"], "c2");
    assert_eq!(updates[8]["status"], "failed");
}

#[test]
fn cli_acp_cell_changes_between_pictures() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    std::fs::write(dir.join("kept.txt"), "same").unwrap();
    std::fs::write(dir.join("gone.txt"), "bye").unwrap();
    std::fs::write(dir.join("edit.txt"), "v1").unwrap();
    let mut tracker = Tracker::default();
    // No picture held: nothing to report.
    assert_eq!(tracker.cell_ended(&dir), FileChanges::default());
    tracker.end();
    tracker.begin(&dir);
    std::fs::write(dir.join("new.txt"), "fresh\n").unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    std::fs::write(dir.join("edit.txt"), "v2 longer").unwrap();
    let changes = tracker.cell_ended(&dir);
    assert_eq!(changes.changed, vec![dir.join("new.txt"), dir.join("edit.txt")]);
    assert_eq!(changes.deleted, vec![dir.join("gone.txt")]);
    // A new file diffs from nothing; a file never seen before has only its location.
    assert_eq!(
        changes.diffs,
        vec![json!({"type": "diff", "path": dir.join("new.txt"), "oldText": null, "newText": "fresh\n"})]
    );
    // The next cell measures from the last picture, and diffs a file seen before against its text then.
    std::fs::write(dir.join("edit.txt"), "v3 even longer").unwrap();
    let changes = tracker.cell_ended(&dir);
    assert_eq!(changes.changed, vec![dir.join("edit.txt")]);
    assert_eq!(
        changes.diffs,
        vec![
            json!({"type": "diff", "path": dir.join("edit.txt"), "oldText": "v2 longer", "newText": "v3 even longer"})
        ]
    );
    // Nothing changed: nothing reported.
    assert_eq!(tracker.cell_ended(&dir), FileChanges::default());
    // `begin` keeps a held picture; `end` drops it.
    std::fs::write(dir.join("later.txt"), "x").unwrap();
    tracker.begin(&dir);
    assert_eq!(tracker.cell_ended(&dir).changed, vec![dir.join("later.txt")]);
    tracker.end();
    std::fs::write(dir.join("later.txt"), "xy").unwrap();
    assert!(tracker.cell_ended(&dir).is_empty());
}
