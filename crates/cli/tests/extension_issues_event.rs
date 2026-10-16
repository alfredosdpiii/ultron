//! The session's `[Extension issues]` answer as the TUI's event: load errors (type `error`) keep their kind, so the
//! TUI draws them in the error color, ahead of the shortcut warnings.

use interactive::lane::LaneEvent;
use serde_json::json;

#[test]
fn cli_extension_issues_answer_keeps_load_errors_apart_from_warnings() {
    let answer = json!({
        "shortcuts": [],
        "diagnostics": [
            {"type": "error", "message": "Invalid extension manifest: x", "path": "/a/extension.json"},
            {"type": "warning", "message": "conflicts", "path": "/b/extension.json"},
        ],
    });
    let LaneEvent::ExtensionShortcuts { diagnostics, .. } = cli::interactive::extension_shortcuts_event(Some(&answer))
    else {
        panic!("not the shortcuts event");
    };
    let kinds: Vec<(&str, bool)> = diagnostics.iter().map(|d| (d.path.as_str(), d.error)).collect();
    assert_eq!(kinds, vec![("/a/extension.json", true), ("/b/extension.json", false)]);
    assert_eq!(diagnostics[0].message, "Invalid extension manifest: x");
}
