//! The interactive client marks the tree entries that have a working-tree checkpoint (with a commit) for `/tree`'s
//! restore key; skipped checkpoints and other entries stay unmarked, and without checkpoints the tree is unchanged.

use serde_json::json;

#[test]
fn cli_checkpoint_tree_marks_entries_with_a_commit() {
    let tree = json!([{"entry": {"id": "u1"}, "children": [
        {"entry": {"id": "a1"}, "children": [
            {"entry": {"id": "u2"}, "children": []},
            {"entry": {"id": "u3"}, "children": []}]}]}]);
    let checkpoints = json!({"checkpoints": [
        {"key": "u1", "kind": "turn", "entryId": "u1", "commit": "c1"},
        {"key": "u2", "kind": "turn", "entryId": "u2", "commit": null, "skipped": "too many files"},
        {"key": "restore:x", "kind": "restore", "entryId": "u3", "commit": "c3"},
    ]});
    let marked = cli::interactive::mark_checkpoints(tree.clone(), &checkpoints);
    assert_eq!(
        marked,
        json!([{"entry": {"id": "u1"}, "checkpoint": true, "children": [
            {"entry": {"id": "a1"}, "children": [
                {"entry": {"id": "u2"}, "children": []},
                {"entry": {"id": "u3"}, "checkpoint": true, "children": []}]}]}])
    );
    assert_eq!(
        cli::interactive::mark_checkpoints(tree.clone(), &json!({"checkpoints": []})),
        tree
    );
    assert_eq!(
        cli::interactive::mark_checkpoints(tree.clone(), &serde_json::Value::Null),
        tree
    );
}
