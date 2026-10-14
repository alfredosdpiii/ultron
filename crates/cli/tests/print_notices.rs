//! Print mode shows the Claude subscription route notice (the `claude_subscription_route` diagnostic's
//! `details.notice` on an assistant message: "Claude subscription limit reached; using API credits until <time>")
//! on stderr: `print_mode::route_notices` reads the notices of the assistant messages a session file gained during
//! the run (after the file's size before it), each once.

use cli::print_mode::route_notices;
use serde_json::json;

fn assistant(text: &str, notice: Option<&str>) -> serde_json::Value {
    let diagnostics = notice.map_or(
        json!([]),
        |n| json!([{"type": "claude_subscription_route", "timestamp": 1, "details": {"route": "api", "notice": n}}]),
    );
    json!({"kind": "entry", "type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": text}],
        "stopReason": "stop", "timestamp": 1, "diagnostics": diagnostics}})
}

#[test]
fn cli_print_route_notices_are_read_from_the_runs_new_messages_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let old = "Claude subscription limit reached; using API credits until 2026-09-21T15:13:20.000Z";
    let header = json!({"kind": "header", "id": "s"}).to_string();
    let before = format!("{header}\n{}\n", json!([assistant("earlier", Some(old))]));
    std::fs::write(&path, &before).unwrap();
    // The run's own writes: two answers on the API (the same notice once) and one back on the subscription.
    let back = "Claude subscription available again; back on the subscription";
    let run = format!(
        "{}\n{}\n{}\n",
        json!([
            assistant("one", Some(old)),
            json!({"kind": "value", "namespace": "x", "op": "set", "value": 1})
        ]),
        json!([assistant("two", Some(old))]),
        json!([assistant("three", Some(back)), assistant("plain", None)]),
    );
    std::fs::write(&path, format!("{before}{run}")).unwrap();
    assert_eq!(route_notices(&path, before.len() as u64), [old, back]);
    // Nothing new: nothing shown; a user message's diagnostic is never a route notice.
    assert!(route_notices(&path, (before.len() + run.len()) as u64).is_empty());
    let user = json!([{"kind": "entry", "type": "message", "message": {"role": "user", "content": "hi",
        "diagnostics": [{"type": "claude_subscription_route", "details": {"notice": "x"}}]}}]);
    let only_user = dir.path().join("user.jsonl");
    std::fs::write(&only_user, format!("{header}\n{user}\n")).unwrap();
    assert!(route_notices(&only_user, 0).is_empty());
    // A missing file has none.
    assert!(route_notices(&dir.path().join("missing.jsonl"), 0).is_empty());
}
