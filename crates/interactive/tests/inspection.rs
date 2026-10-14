//! Replays crates/interactive/fixtures/inspection.json, recorded by fixtures/generate-inspection.mjs from the
//! TypeScript inspection commands (experimental/services/inspection-commands.ts): each command's argument text maps
//! to the same read-only request (or the same usage error), and `formatInspection` renders the same text.

use interactive::inspection::{INSPECTORS, format_inspection, inspection_request};
use serde_json::{Value, json};
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/inspection.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn interactive_inspection_commands_make_the_typescript_requests_and_show_the_same_text() {
    let fixture = fixture();
    let cases = fixture["commands"].as_array().unwrap();
    assert_eq!(cases.len(), INSPECTORS.len() * 9);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let args = case["args"].as_str().unwrap();
        let request = inspection_request(name, args.trim()).expect("an inspection command");
        match (&case["error"], request) {
            (Value::Null, Ok((kind, payload))) => {
                assert_eq!(
                    json!([{"type": kind, "payload": payload}]),
                    case["requests"],
                    "/{name} {args:?}"
                );
                // The control answers every request with the same value; the status is its rendering.
                let answer = json!({"tasks": [{"id": "task-1", "definition": "rlm-frame@1", "state": "completed"}],
                    "usage": {"modelCalls": 2, "cost": 0.0125}});
                let title = format!("/{name} {args}");
                assert_eq!(
                    format_inspection(title.trim(), &answer),
                    case["shown"],
                    "/{name} {args:?}"
                );
            }
            (Value::String(error), Err(usage)) => {
                assert_eq!(&usage, error, "/{name} {args:?}");
                assert_eq!(case["requests"], json!([]));
            }
            (expected, got) => panic!("/{name} {args:?}: expected {expected}, got {got:?}"),
        }
    }
    assert!(inspection_request("model", "").is_none());
}

#[test]
fn interactive_format_inspection_matches_typescript() {
    for case in fixture()["formats"].as_array().unwrap() {
        let title = case["title"].as_str().unwrap();
        assert_eq!(format_inspection(title, &case["value"]), case["text"], "{title}");
    }
}

/// A host answer is built in Rust insertion order, which JSON from TypeScript never shows: `Object.entries` still
/// lists array-index keys first, ascending (recorded with node from `formatInspection`).
#[test]
fn interactive_format_inspection_lists_index_keys_first_whatever_the_insertion_order() {
    let mut map = serde_json::Map::new();
    map.insert("b".into(), json!(1));
    map.insert("10".into(), json!("ten"));
    map.insert("2".into(), json!("two"));
    map.insert("a".into(), json!({}));
    assert_eq!(
        format_inspection("/t", &Value::Object(map)),
        "/t\n2: two\n10: ten\nb: 1\na: (none)"
    );
}
