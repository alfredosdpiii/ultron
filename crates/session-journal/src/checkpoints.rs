//! Working-tree checkpoints of a session (Rust-only; no TypeScript reference): one scalar value per checkpoint in
//! the `ultron.checkpoint` namespace, so a TypeScript host reading the file keeps them as opaque values.
//!
//! A turn checkpoint is keyed by the root user turn's prompt entry id and records the working tree as it was when
//! that turn started (`kind: "turn"`); a restore first saves the files it replaces as a `kind: "restore"` checkpoint
//! keyed `restore:<id>`, attached to the branch tip of the moment (`entryId`), so the restore is itself undoable. A
//! turn whose tree could not be recorded keeps a value with `skipped` (the note) and no commit.
//!
//! Value: `{kind, entryId, vcs, root, head, commit, tree?, excluded, createdAt, skipped?, restoredTo?}`.

use crate::session::Session;
use crate::state::Write;
use serde_json::Value;

pub const CHECKPOINTS: &str = "ultron.checkpoint";
pub const RESTORE_PREFIX: &str = "restore:";

/// Record checkpoint `key`.
pub fn put(session: &mut Session, key: &str, value: Value) -> Result<(), String> {
    session.commit(&[Write::set(CHECKPOINTS, key, value)]).map(|_| ())
}

/// Every checkpoint as `(key, value)`, oldest first (by the sequence of its write).
pub fn list(session: &Session) -> Vec<(String, Value)> {
    let mut found: Vec<_> = session
        .state()
        .scan_values(CHECKPOINTS, "")
        .into_iter()
        .map(|v| (v.seq, v.key.clone(), v.value.clone()))
        .collect();
    found.sort_by_key(|(seq, ..)| *seq);
    found.into_iter().map(|(_, key, value)| (key, value)).collect()
}

/// The newest checkpoint that holds a commit (what an unchanged tree reuses).
pub fn latest_taken(session: &Session) -> Option<Value> {
    list(session)
        .into_iter()
        .rev()
        .map(|(_, value)| value)
        .find(|value| value["commit"].is_string())
}

/// The checkpoint that describes the files at `entry_id`: the newest one attached to it, else to the nearest entry
/// above it (the start of the turn it belongs to). `None`: no checkpoint on its path.
pub fn for_entry(session: &Session, entry_id: &str) -> Option<(String, Value)> {
    let all = list(session);
    let state = session.state();
    let mut current = Some(entry_id.to_string());
    // A cycle cannot occur in a valid journal; the bound only guards a corrupt one.
    for _ in 0..=state.entries().len() {
        let id = current?;
        if let Some(found) = all.iter().rev().find(|(_, value)| value["entryId"] == id.as_str()) {
            return Some(found.clone());
        }
        current = state
            .entry(&id)
            .and_then(|entry| entry["parentId"].as_str())
            .map(str::to_string);
    }
    None
}
