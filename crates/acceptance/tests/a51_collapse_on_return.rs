//! A51: after 20 completed tasks the root's next request stays bounded (results collapse on return by context
//! edit) while `agents.result` still returns full values.
//! Ported from packages/coding-agent/test/ultron-a51-collapse-on-return.test.ts through the `ultron` binary over RPC
//! with the scripted provider: each root cell invokes `rlm-child@1` and prints its 4 KB value; once the model has
//! answered after a cell's output the root lane collapses it to one line per task, so request size stops growing,
//! and the last cell fetches all 20 values in full through the journal.
//! Also from packages/coding-agent/test/ultron-context-control.test.ts: the three cases the manifest names (edits
//! keep tool calls and results paired; collapse lines), with literal expectations, over
//! `kernel_apis::context_control`; the guide line is checked in the system prompt the `ultron` binary sends.

mod common;

use common::{HANG, Install, Provider, Reply, Request, stdout_of};
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

const TASKS: usize = 20;
const VALUE_BYTES: usize = 4000;
const ROOT_PROMPT: &str = "A51: run twenty tasks and keep going";

/// A child's 4 KB answer, distinct per task so a leak of any one of them is visible.
fn child_value(index: usize) -> String {
    format!("RESULT_{index}_")
        .repeat(VALUE_BYTES)
        .chars()
        .take(VALUE_BYTES)
        .collect()
}

/// The task index a child's request is for (`CHILD <n>` in its prompt).
fn child_index(request: &Request) -> Option<usize> {
    request.users().iter().find_map(|user| {
        let rest = &user[user.find("CHILD ")? + "CHILD ".len()..];
        rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
    })
}

fn is_root(request: &Request) -> bool {
    request.users().iter().any(|user| user == ROOT_PROMPT)
}

fn a51_script(request: &Request) -> Reply {
    if let Some(index) = child_index(request) {
        return Reply::Text(child_value(index));
    }
    if !is_root(request) {
        return Reply::Text("unexpected".into());
    }
    // The root's turn: the cells it already ran. Each cell has its own call id, as a real model's would.
    let turn = request.tool_results().len();
    let call = format!("call_a51_{turn}");
    if turn < TASKS {
        return Reply::RlmCall(
            call,
            format!(
                "r{turn} = await agents.invoke(\"rlm-child@1\", {{\"prompt\": \"CHILD {turn}\"}}, key=\"a51-{turn}\")\nprint(r{turn})"
            ),
        );
    }
    if turn == TASKS {
        return Reply::RlmCall(
            call,
            [
                "tasks = [t for t in (await agents.tasks())[\"tasks\"] if t[\"definition\"] == \"rlm-child@1\"]",
                "values = [await agents.result(t['id']) for t in tasks]",
                "print('FULL', len(values), sum(len(v['value']) for v in values), sorted(v['value'][:12] for v in values)[:2])",
            ]
            .join("\n"),
        );
    }
    Reply::Text("A51_DONE".into())
}

#[test]
fn acceptance_a51_twenty_completed_tasks_keep_the_next_request_bounded_and_values_full_through_agents_result() {
    let provider = Provider::start(a51_script);
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client.prompt_and_wait(ROOT_PROMPT, HANG).unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("A51_DONE"));
    let messages = client.get_messages().unwrap();
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));

    let roots: Vec<Request> = provider.requests().into_iter().filter(is_root).collect();
    assert_eq!(roots.len(), TASKS + 2);
    let sizes: Vec<usize> = roots.iter().map(|request| request.raw.len()).collect();
    let base = sizes[0];
    // Without collapse every request would carry every earlier 4 KB value (the last one over 80 KB more). With it a
    // request carries the newest cell or two in full and one line per earlier task.
    let bound = base + 3 * (VALUE_BYTES + 600) + TASKS * 900;
    for (turn, size) in sizes.iter().enumerate() {
        assert!(*size < bound, "request {turn}: {size} bytes, bound {bound}");
    }
    assert!(sizes[TASKS + 1] - base < TASKS * VALUE_BYTES / 2);

    let last = &roots[TASKS + 1].raw;
    // Early results are gone from the model's view (a 200-character head stays); one-line summaries and handles
    // remain.
    for index in [0, 1, 5, 10] {
        assert!(!last.contains(&child_value(index)[..600]), "task {index} leaked");
    }
    assert!(last.contains("collapsed on return"));
    assert!(last.contains("rlm-child@1 key=a51-0 succeeded"));
    assert!(last.contains("await agents.result(\\\""));
    // The journal still returns every full value.
    assert!(last.contains(&format!("FULL {TASKS} {}", TASKS * VALUE_BYTES)));
    // The durable transcript keeps every cell's output in full.
    let transcript = serde_json::to_string(&messages).unwrap();
    for index in [0, 1, 5, 10, 19] {
        assert!(
            transcript.contains(&child_value(index)[..600]),
            "task {index} not in the transcript"
        );
    }
}
