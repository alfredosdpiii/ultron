//! `ultron usage` over session files whose durable records hold sub-agent conversations: each child response is read
//! from its `pi.assistant` entry (model, input/output/cache split, cost); the host's ledger only adds what the records
//! lack, as a totals-only row; a session without durable records reports from the ledger as before.

use cli::usage::log::read_session_log;
use cli::usage::report::{DURABLE_CHILDREN_MODEL, build_session_report};
use serde_json::{Value, json};
use std::path::Path;

fn usage(input: u64, output: u64, cache_read: u64, cost: f64) -> Value {
    json!({"input": input, "output": output, "cacheRead": cache_read, "cacheWrite": 0,
        "totalTokens": input + output + cache_read, "cost": {"total": cost}})
}

/// The root's one transcript response (on `main`), 10 in / 5 out.
fn root_lines() -> Vec<Value> {
    vec![
        json!({"kind": "entry", "id": "e1", "parentId": null, "timestamp": 1001, "type": "message",
            "message": {"role": "assistant", "provider": "anthropic", "model": "claude-sonnet-4-5", "content": [],
                "usage": usage(10, 5, 0, 0.5)}}),
        json!({"kind": "value", "op": "set", "seq": 2, "namespace": "pi.branch.tip", "key": "main", "value": "e1"}),
    ]
}

/// One durable commit (an `ultron.module/durable` list append).
fn durable(seq: u64, writes: Value) -> Value {
    json!({"kind": "list", "op": "append", "seq": 100 + seq, "namespace": "ultron.module", "key": "durable",
        "value": {"seq": seq, "writes": writes}})
}

fn assistant(id: u64, conversation: u64, model: &str, usage: Value) -> Value {
    json!({"type": "entry", "value": {"id": id, "conversationId": conversation, "kind": "pi.assistant",
        "model": [{"role": "assistant", "provider": "anthropic", "model": model, "content": [], "usage": usage}]}})
}

/// The ledger's document with one root's tree turns.
fn ledger(count: u64, tokens: f64, cost: f64) -> Value {
    json!({"kind": "value", "op": "set", "seq": 90, "namespace": "ultron.usage", "key": "root", "value": {
        "version": 1, "roots": {"turn:op1": {"rootId": "turn:op1", "startedAt": null, "deadlineAt": null,
            "reservations": [], "calls": [],
            "turns": {"count": count, "tokens": tokens, "lastAt": 2000, "cost": cost, "unpriced": 0}}}}})
}

fn report_of(dir: &Path, lines: &[Value]) -> Value {
    let path = dir.join("session.jsonl");
    let mut text = String::from(r#"{"v":4,"kind":"header","id":"s1","storageVersion":1,"createdAt":1000,"cwd":"/x"}"#);
    text.push('\n');
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    build_session_report(&read_session_log(&path).unwrap())
}

fn split(bucket: &Value) -> [f64; 6] {
    ["responses", "input", "output", "cacheRead", "cacheWrite", "totalTokens"]
        .map(|field| bucket[field].as_f64().unwrap_or(f64::NAN))
}

fn models(report: &Value) -> Vec<(String, [f64; 6], f64)> {
    report["usage"]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["model"].as_str().unwrap().to_string(),
                split(m),
                m["cost"]["reportedUsd"].as_f64().unwrap_or(0.0),
            )
        })
        .collect()
}

#[test]
fn cli_usage_durable_children_are_reported_per_model_with_their_split() {
    let dir = tempfile::tempdir().unwrap();
    let mut lines = root_lines();
    lines.extend([
        // The root conversation's own responses are transcript entries: its durable copy is not counted again.
        durable(
            1,
            json!([{"type": "conversation", "value": {"id": 1}},
                assistant(2, 1, "claude-sonnet-4-5", usage(10, 5, 0, 0.5))]),
        ),
        // A spawned child (task 15 owns conversation 16) on haiku, two responses; a child on the root's model.
        durable(
            2,
            json!([
                {"type": "task", "value": {"id": 15, "conversationId": 1, "kind": "ultron.child",
                    "input": {"name": "ultron.rlm-child.ultron-task-a"}}},
                {"type": "conversation", "value": {"id": 16, "owner": {"conversationId": 1, "taskId": 15}}},
                {"type": "task", "value": {"id": 20, "conversationId": 1, "kind": "ultron.child",
                    "input": {"name": "ultron.rlm-child.ultron-task-b"}}},
                {"type": "conversation", "value": {"id": 21, "owner": {"conversationId": 1, "taskId": 20}}},
            ]),
        ),
        durable(
            3,
            json!([assistant(17, 16, "claude-haiku-4-5", usage(100, 20, 30, 0.1))]),
        ),
        durable(
            4,
            json!([assistant(18, 16, "claude-haiku-4-5", usage(50, 10, 0, 0.05))]),
        ),
        durable(5, json!([assistant(22, 21, "claude-sonnet-4-5", usage(7, 3, 0, 0.25))])),
        // Another root conversation of the session (no owner): "other", not a sub-agent.
        durable(
            6,
            json!([{"type": "conversation", "value": {"id": 30}},
                assistant(31, 30, "claude-opus-4-5", usage(4, 1, 0, 1.0))]),
        ),
        json!({"kind": "value", "op": "set", "seq": 80, "namespace": "ultron.tasks", "key": "root", "value": {
        "version": 1, "tasks": [
            {"id": "ultron-task-a", "definition": "rlm-child@1", "state": "completed"},
            {"id": "ultron-task-b", "definition": "rlm-child@1", "state": "completed"},
        ]}}),
        // A conversation whose owning task the file does not name is still a sub-agent's.
        durable(
            7,
            json!([{"type": "conversation", "value": {"id": 40, "owner": {"conversationId": 1, "taskId": 41}}},
                assistant(42, 40, "claude-haiku-4-5", usage(2, 1, 0, 0.01))]),
        ),
        // The ledger counted exactly these responses: nothing is left for a totals-only row.
        ledger(
            6,
            15.0 + 150.0 + 60.0 + 10.0 + 5.0 + 3.0,
            0.5 + 0.15 + 0.25 + 1.0 + 0.01,
        ),
    ]);
    let report = report_of(dir.path(), &lines);
    let lanes = &report["usage"]["lanes"];
    assert_eq!(split(&lanes["root"]), [1.0, 10.0, 5.0, 0.0, 0.0, 15.0], "{lanes}");
    assert_eq!(
        split(&lanes["subagents"]),
        [4.0, 159.0, 34.0, 30.0, 0.0, 223.0],
        "{lanes}"
    );
    assert_eq!(split(&lanes["other"]), [1.0, 4.0, 1.0, 0.0, 0.0, 5.0], "{lanes}");
    assert_eq!(
        split(&report["usage"]["total"]),
        [6.0, 173.0, 40.0, 30.0, 0.0, 243.0],
        "{report}"
    );
    let mut rows = models(&report);
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        rows,
        vec![
            (
                "anthropic/claude-haiku-4-5".into(),
                [3.0, 152.0, 31.0, 30.0, 0.0, 213.0],
                0.16000000000000003
            ),
            ("anthropic/claude-opus-4-5".into(), [1.0, 4.0, 1.0, 0.0, 0.0, 5.0], 1.0),
            (
                "anthropic/claude-sonnet-4-5".into(),
                [2.0, 17.0, 8.0, 0.0, 0.0, 25.0],
                0.75
            ),
        ],
        "{report}"
    );
    // Each child is listed under its own model, with its tokens.
    assert_eq!(
        report["depth"]["subagents"]["byModel"],
        json!([
            {"model": "anthropic/claude-haiku-4-5", "count": 1, "tokens": 210, "unmeasured": 0},
            {"model": "anthropic/claude-sonnet-4-5", "count": 1, "tokens": 10, "unmeasured": 0},
        ]),
        "{report}"
    );
}

#[test]
fn cli_usage_ledger_turns_beyond_the_durable_records_stay_a_totals_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut lines = root_lines();
    lines.extend([
        durable(
            1,
            json!([
                {"type": "task", "value": {"id": 15, "conversationId": 1, "kind": "ultron.child",
                    "input": {"name": "ultron.rlm-child.ultron-task-a"}}},
                {"type": "conversation", "value": {"id": 16, "owner": {"conversationId": 1, "taskId": 15}}},
                assistant(17, 16, "claude-haiku-4-5", usage(100, 20, 0, 0.1)),
            ]),
        ),
        // A root conversation's response, outside the sub-agents lane but counted by the ledger too.
        durable(
            2,
            json!([{"type": "conversation", "value": {"id": 30}},
                assistant(31, 30, "claude-opus-4-5", usage(4, 1, 0, 1.0))]),
        ),
        // Two more responses (40 tokens, $0.2) the records do not hold, e.g. a child's compaction summaries.
        ledger(5, 15.0 + 120.0 + 5.0 + 40.0, 0.5 + 0.1 + 1.0 + 0.2),
    ]);
    let report = report_of(dir.path(), &lines);
    let rows = models(&report);
    let leftover = rows
        .iter()
        .find(|r| r.0 == DURABLE_CHILDREN_MODEL)
        .expect("a totals row");
    assert_eq!(leftover.1, [2.0, 0.0, 0.0, 0.0, 0.0, 40.0], "{report}");
    assert!((leftover.2 - 0.2).abs() < 1e-9, "{report}");
    assert_eq!(
        split(&report["usage"]["lanes"]["subagents"]),
        [3.0, 100.0, 20.0, 0.0, 0.0, 160.0]
    );
    // The totals equal the ledger's.
    assert_eq!(report["usage"]["total"]["totalTokens"], json!(180), "{report}");
    assert_eq!(report["usage"]["total"]["responses"], json!(5), "{report}");
}

/// A session written before the durable records (or by a host without them): the ledger's turns beyond the
/// transcript are its children's usage, a totals-only row as before.
#[test]
fn cli_usage_a_session_without_durable_records_reports_children_from_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let mut lines = root_lines();
    lines.push(ledger(3, 15.0 + 30.0, 0.5 + 0.3));
    let report = report_of(dir.path(), &lines);
    let rows = models(&report);
    assert_eq!(
        rows.iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(),
        vec![
            (DURABLE_CHILDREN_MODEL, [2.0, 0.0, 0.0, 0.0, 0.0, 30.0]),
            ("anthropic/claude-sonnet-4-5", [1.0, 10.0, 5.0, 0.0, 0.0, 15.0]),
        ],
        "{report}"
    );
    assert!((rows[0].2 - 0.3).abs() < 1e-9, "{report}");
    assert_eq!(
        split(&report["usage"]["lanes"]["subagents"]),
        [2.0, 0.0, 0.0, 0.0, 0.0, 30.0]
    );
}
