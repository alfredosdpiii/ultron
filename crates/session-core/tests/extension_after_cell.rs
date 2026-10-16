//! Extensions' `after_cell_changes` as an after-cell file hook (legacy-extension-adapter.ts `fileGuards`): the event
//! carries the cell's files, checked writes, deletions, completeness, directory and lane; a handler's message is a
//! finding delivered with the lane's next cell; nothing runs while no extension handles the event.

use host::file_hooks::{AfterOutcome, CellChanges, FileWriteGuard};
use serde_json::{Value, json};
use session_core::extension_file_hooks::ExtensionAfterCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[test]
fn session_core_extension_after_cell_changes_event_and_message() {
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = events.clone();
    let guard = ExtensionAfterCell::with(
        || true,
        move |event| {
            seen.lock().unwrap().push(event.clone());
            Some(json!({ "message": "review notes.txt" }))
        },
    );
    assert!(guard.watches_cells());
    assert_eq!(guard.name(), "extension");
    let changes = CellChanges {
        files: vec![PathBuf::from("/w/notes.txt")],
        checked: vec![PathBuf::from("/w/app.py")],
        deleted: vec![PathBuf::from("/w/old.txt")],
        complete: true,
    };
    let report = guard
        .after_cell_changes(&changes, Path::new("/w"), "main")
        .unwrap()
        .unwrap();
    assert_eq!(report.message.as_deref(), Some("review notes.txt"));
    assert_eq!(report.outcome, AfterOutcome::Findings);
    assert_eq!(
        events.lock().unwrap()[0],
        json!({ "type": "after_cell_changes", "files": ["/w/notes.txt"], "checked": ["/w/app.py"],
                "deleted": ["/w/old.txt"], "complete": true, "cwd": "/w", "lane": "main" })
    );
    // No message (or an empty one): nothing to report.
    let quiet = ExtensionAfterCell::with(|| true, |_| Some(json!({ "message": "" })));
    assert_eq!(
        quiet.after_cell_changes(&changes, Path::new("/w"), "main").unwrap(),
        None
    );
    let none = ExtensionAfterCell::with(|| true, |_| None);
    assert_eq!(
        none.after_cell_changes(&changes, Path::new("/w"), "main").unwrap(),
        None
    );
    // Without a handler the guard does not watch cells (no snapshots are taken for it).
    assert!(!ExtensionAfterCell::with(|| false, |_| None).watches_cells());
}
