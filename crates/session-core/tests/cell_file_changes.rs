//! An rlm cell's result carries the diff of the files it wrote through the checked `write()`/`edit()` in
//! `details.fileChanges` (session_core::cell_file_changes over host::cell_writes), in the product's session: a new
//! file, an edited one, a blocked-or-unchanged write and a cell that writes nothing (whose details stay `{}`, as in
//! TypeScript). The interactive mode draws it under the expanded cell.

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::{Value, json};

/// Every toolResult message the session committed, in order.
fn tool_results(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.get("role") == Some(&json!("toolResult")) {
                out.push(value.clone());
                return;
            }
            map.values().for_each(|v| tool_results(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| tool_results(v, out)),
        _ => {}
    }
}

#[test]
fn session_core_cell_results_carry_the_diff_of_the_files_the_cell_wrote() {
    let edit_cell = [
        "await write('notes/new.txt', 'first\\nsecond\\n')",
        "await edit('src/app.py', old_str='return 1', new_str='return 2')",
        // A write that changes nothing is not reported.
        "await write('same.txt', 'same\\n')",
        "print('done')",
    ]
    .join("\n");
    let script = in_order(vec![
        tool("toolu_1", &edit_cell),
        tool("toolu_2", "print('nothing written')"),
        tool("toolu_3", "await write('keys.env', 'KEY=AKIAQ7XRJ2M4PL3ZWNVB\\n')"),
        text("finished"),
    ]);
    let app = "def f():\n    return 1\n\n\ndef g():\n    return 3\n";
    let a = actor(
        script,
        Setup {
            files: vec![
                ("src/app.py", app.as_bytes().to_vec()),
                ("same.txt", b"same\n".to_vec()),
            ],
            // Loki advises instead of blocking, so the secret is written and its diff must be masked.
            vars: vec![("ULTRON_LOKI", "advise".into())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("edit files");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    assert_eq!(
        std::fs::read_to_string(a.cwd.join("src/app.py")).unwrap(),
        app.replace("return 1", "return 2")
    );
    let mut results = Vec::new();
    for write in a.writes() {
        tool_results(&write, &mut results);
    }
    let by_call = |id: &str| {
        results
            .iter()
            .rev()
            .find(|r| r["toolCallId"] == id)
            .unwrap_or_else(|| panic!("no result for {id}: {results:?}"))
            .clone()
    };
    let first = by_call("toolu_1");
    assert_eq!(first["isError"], false, "{first}");
    assert_eq!(
        first["details"],
        json!({"fileChanges": [
            {"path": "notes/new.txt", "created": true, "diff": "+1 first\n+2 second"},
            {"path": "src/app.py", "created": false,
             "diff": " 1 def f():\n-2     return 1\n+2     return 2\n 3 \n 4 \n   ..."},
        ]})
    );
    assert_eq!(by_call("toolu_2")["details"], json!({}));
    // Secrets are masked in the diff as in the cell's output (the file itself keeps them).
    let keys = by_call("toolu_3")["details"]["fileChanges"][0].clone();
    assert_eq!(keys["path"], "keys.env", "{keys}");
    let diff = keys["diff"].as_str().unwrap();
    assert!(
        diff.starts_with("+1 KEY=") && !diff.contains("AKIAQ7XRJ2M4PL3ZWNVB"),
        "{diff}"
    );
    assert!(diff.contains("[REDACTED"), "{diff}");
    assert!(
        std::fs::read_to_string(a.cwd.join("keys.env"))
            .unwrap()
            .contains("AKIAQ7XRJ2M4PL3ZWNVB")
    );
}
