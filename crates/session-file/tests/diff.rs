//! Replays fixtures/diff.json (generate-diff.mjs): jsdiff 8.0.4's `diffLines` and the TypeScript edit tool's
//! `generateDiffString` at context 4 and 2, compared exactly.

use serde_json::{Value, json};
use session_file::diff::{diff_lines, generate_diff_string};

fn cases() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/diff.json")).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}

#[test]
fn session_file_diff_lines_matches_jsdiff() {
    for case in cases() {
        let (old, new) = (case["old"].as_str().unwrap(), case["new"].as_str().unwrap());
        let changes: Vec<Value> = diff_lines(old, new)
            .into_iter()
            .map(|c| json!({"value": c.value, "added": c.added, "removed": c.removed}))
            .collect();
        assert_eq!(Value::Array(changes), case["lines"], "{case}");
    }
}

#[test]
fn session_file_generate_diff_string_matches_the_edit_tool() {
    let cases = cases();
    assert_eq!(cases.len(), 20);
    for case in cases {
        let (old, new) = (case["old"].as_str().unwrap(), case["new"].as_str().unwrap());
        for (context, key) in [(4, "context4"), (2, "context2")] {
            let (diff, first) = generate_diff_string(old, new, context);
            // `firstChangedLine: undefined` is left out of the recorded JSON.
            let mut got = json!({ "diff": diff });
            if let Some(first) = first {
                got["firstChangedLine"] = json!(first);
            }
            assert_eq!(got, case[key], "{key} of {case}");
        }
    }
}
