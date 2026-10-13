//! A51 (partial): after 20 completed tasks the root's next request stays bounded (results collapse on return by
//! context edit) while `agents.result` still returns full values.
//! Ported from packages/coding-agent/test/ultron-context-control.test.ts: the three cases the manifest names (edits
//! keep tool calls and results paired; collapse lines), with literal expectations, over
//! `kernel_apis::context_control`; the guide line is checked in the system prompt the `ultron` binary sends.
//! Not ported: ultron-a51-collapse-on-return.test.ts needs `agents.invoke` with the `rlm-child@1` definition and
//! `agents.result` (no definition registry in Rust), and `ContextControl` is not wired into the session's lanes
//! (crates/session-core/src/compose.rs), so nothing collapses a cell output in a live run.

mod common;

use common::{Install, Provider, Reply, stdout_of};
use kernel_apis::context_control::{
    CONTEXT_EDIT_CUSTOM_TYPE, CONTEXT_OMITTED_CUSTOM_TYPE, CONTEXT_OMITTED_TOOL_RESULT_TEXT, TaskLine,
    apply_context_edits, collapsed_cell_text, message_text, task_line,
};
use serde_json::{Value, json};

fn message(id: &str, seq: i64, message: Value) -> Value {
    json!({"id": id, "parentId": null, "seq": seq, "timestamp": seq, "type": "message", "message": message})
}

fn edit(id: &str, seq: i64, data: Value) -> Value {
    json!({"id": id, "parentId": null, "seq": seq, "timestamp": seq, "type": "custom",
        "customType": CONTEXT_EDIT_CUSTOM_TYPE, "data": data})
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text, "timestamp": 0})
}

fn assistant(text: &str, calls: &[&str]) -> Value {
    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend(
        calls
            .iter()
            .map(|id| json!({"type": "toolCall", "id": id, "name": "rlm", "arguments": {"code": "x"}})),
    );
    json!({"role": "assistant", "content": content, "api": "openai-completions", "provider": "faux",
        "model": "faux", "stopReason": if calls.is_empty() { "stop" } else { "toolUse" }, "timestamp": 0})
}

fn result(call: &str, text: &str) -> Value {
    json!({"role": "toolResult", "toolCallId": call, "toolName": "rlm",
        "content": [{"type": "text", "text": text}], "isError": false, "timestamp": 0})
}

fn base() -> Vec<Value> {
    vec![
        message("u1", 1, user("first")),
        message("a1", 2, assistant("calling", &["c1", "c2"])),
        message("r1", 3, result("c1", "BIG ONE")),
        message("r2", 4, result("c2", "BIG TWO")),
        message("a2", 5, assistant("answer", &[])),
    ]
}

fn visible_ids(entries: &[Value]) -> Vec<String> {
    apply_context_edits(entries)
        .into_iter()
        .map(|applied| applied.entry)
        .filter(|entry| !(entry["type"] == "custom" && entry["customType"] == CONTEXT_OMITTED_CUSTOM_TYPE))
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn acceptance_a51_omitting_an_assistant_message_omits_the_results_of_its_calls() {
    let mut entries = base();
    entries.push(edit(
        "e1",
        6,
        json!({"edits": [{"targetId": "a1", "replacement": null}]}),
    ));
    assert_eq!(visible_ids(&entries), ["u1", "a2", "e1"]);
}

#[test]
fn acceptance_a51_an_omitted_result_whose_call_stays_visible_becomes_a_stub() {
    let mut entries = base();
    entries.push(edit("e1", 6, json!({"targetId": "r1", "replacement": null})));
    let applied = apply_context_edits(&entries);
    let r1 = &applied.iter().find(|a| a.entry["id"] == "r1").unwrap().entry;
    assert_eq!(r1["type"], "message");
    assert_eq!(message_text(&r1["message"]), CONTEXT_OMITTED_TOOL_RESULT_TEXT);
    assert_eq!(CONTEXT_OMITTED_TOOL_RESULT_TEXT, "[Tool result removed from context.]");
    // The pair stays: the call's assistant message and the other result are untouched.
    assert_eq!(visible_ids(&entries), ["u1", "a1", "r1", "r2", "a2", "e1"]);
}

#[test]
fn acceptance_a51_collapse_lines_one_per_task_with_random_keys_hidden() {
    let line = |id: &str, definition: &str, key: &str, status: &str, cost: Option<f64>| TaskLine {
        id: id.into(),
        definition: definition.into(),
        key: key.into(),
        status: status.into(),
        cost,
    };
    assert_eq!(
        task_line(&line("t1", "rlm-child@1", "fetch-1", "succeeded", Some(0.0123))),
        "↳ task t1 rlm-child@1 key=fetch-1 succeeded cost=$0.0123 · full result: await agents.result(\"t1\")"
    );
    assert_eq!(
        task_line(&line(
            "t2",
            "x@1",
            "0f8fad5b-d9cb-469f-a165-70867728950e",
            "failed",
            None
        )),
        "↳ task t2 x@1 failed cost=? · full result: await agents.result(\"t2\")"
    );
    let lines: Vec<TaskLine> = (0..10)
        .map(|index| {
            line(
                &format!("t{index}"),
                "d@1",
                &format!("k{index}"),
                "succeeded",
                Some(0.0),
            )
        })
        .collect();
    let text = collapsed_cell_text("entry-1", &"x".repeat(5000), &lines);
    assert!(text.contains("ctx.get(\"entry-1\")"));
    assert!(text.contains("+2 more finished tasks"));
    assert!(text.chars().count() < 1500);

    // CONTEXT_PROMPT: the guide the model gets names ctx.forget.
    let provider = Provider::start(|_| Reply::Text("OK".into()));
    let install = Install::new(&provider);
    assert_eq!(stdout_of(&install.prompt("A51 guide", &[])), "OK\n");
    let system = provider.roots()[0].system();
    assert!(system.contains("`ctx` edits your view of your own context (`ctx.history()`, `ctx.forget`"));
}
