//! `buildSessionContext` with `entryProjectors` (packages/agent/src/harness/session/context.ts): a custom entry
//! whose type has a projector contributes its messages at its place in the branch; edits apply first, so an omitted
//! custom entry projects nothing; other custom types still contribute nothing.

use serde_json::{Value, json};
use session_journal::context::{EntryProjector, build_session_context, build_session_context_with};

fn note(entry: &Value) -> Vec<Value> {
    vec![
        json!({"role": "custom", "customType": "note", "content": entry["data"]["text"], "display": true,
        "timestamp": entry["timestamp"]}),
    ]
}

const PROJECTORS: &[(&str, EntryProjector)] = &[("ultron:context-note", note)];

fn path() -> Vec<Value> {
    vec![
        json!({"id": "u1", "parentId": null, "timestamp": 1, "type": "message",
            "message": {"role": "user", "content": "hi", "timestamp": 1}}),
        json!({"id": "n1", "parentId": "u1", "timestamp": 2, "type": "custom", "customType": "ultron:context-note",
            "data": {"text": "first"}}),
        json!({"id": "x1", "parentId": "n1", "timestamp": 3, "type": "custom", "customType": "other",
            "data": {"text": "ignored"}}),
        json!({"id": "n2", "parentId": "x1", "timestamp": 4, "type": "custom", "customType": "ultron:context-note",
            "data": {"text": "second"}}),
    ]
}

#[test]
fn session_journal_context_projectors_place_custom_messages_and_respect_edits() {
    let projected = build_session_context_with(&path(), PROJECTORS);
    assert_eq!(
        projected,
        vec![
            json!({"role": "user", "content": "hi", "timestamp": 1}),
            json!({"role": "custom", "customType": "note", "content": "first", "display": true, "timestamp": 2}),
            json!({"role": "custom", "customType": "note", "content": "second", "display": true, "timestamp": 4}),
        ]
    );
    // Without projectors custom entries contribute nothing.
    assert_eq!(
        build_session_context(&path()),
        vec![json!({"role": "user", "content": "hi", "timestamp": 1})]
    );

    // An edit that omits the first note removes its message; the second stays.
    let mut edited = path();
    edited.push(
        json!({"id": "e1", "parentId": "n2", "timestamp": 5, "type": "custom", "customType": "context_edit",
        "data": {"edits": [{"targetId": "n1", "replacement": null}]}}),
    );
    let texts: Vec<Value> = build_session_context_with(&edited, PROJECTORS)
        .into_iter()
        .map(|m| m["content"].clone())
        .collect();
    assert_eq!(texts, [json!("hi"), json!("second")]);
}
