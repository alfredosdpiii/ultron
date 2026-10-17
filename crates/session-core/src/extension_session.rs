//! `ctx.sessionManager` (Pi's `ReadonlySessionManager`) for extension processes: the session's Pi view
//! (crate::pi_view) and its metadata. Requests (crates/extensions/PROTOCOL.md section 4):
//!
//! | Request | Getter |
//! | --- | --- |
//! | `session/file`, `session/id`, `session/dir`, `session/cwd` | `getSessionFile`, `getSessionId`, `getSessionDir`, `getCwd` |
//! | `session/header` | `getHeader` |
//! | `session/entries`, `session/branch`, `session/leaf`, `session/leafEntry` | `getEntries`, `getBranch`, `getLeafId`, `getLeafEntry` |
//! | `session/entry {id}`, `session/label {id}` | `getEntry`, `getLabel` |
//! | `session/tree` | `getTree` |
//! | `session/contextEntries` | `buildContextEntries` |
//! | `session/getName` | `getSessionName` |
//!
//! The worker's facade (legacy-extension-adapter.ts `createSessionManagerFacade`) answers the file, id and cwd from
//! the session and everything else from an empty in-memory manager (its `getSessionDir` returns the file path).
//! These follow Pi's `SessionManager` on the real session instead, as `session/entries` and `session/branch` do.

use serde_json::{Map, Value, json};
use session_journal::Session;

fn pi_header(session: &Session) -> Value {
    let metadata = session.metadata();
    let mut header = Map::new();
    header.insert("type".into(), json!("session"));
    header.insert("version".into(), json!(3));
    header.insert("id".into(), json!(metadata.id));
    header.insert(
        "timestamp".into(),
        json!(session_file::js::iso_string(metadata.created_at)),
    );
    header.insert("cwd".into(), json!(metadata.cwd));
    if let Some(parent) = &metadata.legacy_parent_session_path {
        header.insert("parentSession".into(), json!(parent));
    }
    Value::Object(header)
}

/// `getTree()` with the session's labels: a node carries `label` (and `labelTimestamp` when the Pi view has it)
/// only while the entry is labelled.
fn labelled_tree(session: &Session, manager: &session_file::SessionManager) -> Value {
    let mut tree = crate::view::tree(manager);
    let mut stack: Vec<&mut Value> = tree.as_array_mut().into_iter().flatten().collect();
    while let Some(node) = stack.pop() {
        let id = node["entry"]["id"].as_str().unwrap_or_default().to_string();
        if let Some(map) = node.as_object_mut() {
            match session.label(&id) {
                None => {
                    map.remove("label");
                    map.remove("labelTimestamp");
                }
                Some(label) if map.get("label").and_then(Value::as_str) != Some(label.as_str()) => {
                    map.insert("label".into(), label.into());
                    map.remove("labelTimestamp");
                }
                Some(_) => {}
            }
        }
        if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
            stack.extend(children.iter_mut());
        }
    }
    tree
}

/// Serves a read-only session request; `None` for any other method.
pub fn request(session: &Session, method: &str, params: &Value) -> Option<Result<Value, String>> {
    let id = || -> Result<String, String> {
        params
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "\"id\" must be a string".to_string())
    };
    let metadata = session.metadata();
    let path = || metadata.path.display().to_string();
    let answer = match method {
        "session/file" => Ok(json!(path())),
        "session/id" => Ok(json!(metadata.id)),
        "session/dir" => Ok(json!(
            metadata
                .path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )),
        "session/cwd" => Ok(json!(metadata.cwd)),
        "session/header" => Ok(pi_header(session)),
        "session/getName" => Ok(json!(session.name())),
        "session/label" => id().map(|id| session.label(&id).map_or(Value::Null, Value::from)),
        "session/entries"
        | "session/branch"
        | "session/leaf"
        | "session/leafEntry"
        | "session/entry"
        | "session/tree"
        | "session/contextEntries" => {
            let pi = crate::pi_view::pi_manager(session);
            match method {
                "session/entries" => Ok(Value::Array(pi.entries().into_iter().cloned().collect())),
                "session/branch" => Ok(Value::Array(pi.branch_entries(None).into_iter().cloned().collect())),
                "session/leaf" => Ok(pi.leaf_id().cloned().unwrap_or(Value::Null)),
                "session/leafEntry" => Ok(pi.leaf_entry().cloned().unwrap_or(Value::Null)),
                "session/entry" => id().map(|id| pi.entry(&id).cloned().unwrap_or(Value::Null)),
                "session/tree" => Ok(labelled_tree(session, &pi)),
                _ => Ok(Value::Array(pi.build_context_entries().into_iter().cloned().collect())),
            }
        }
        _ => return None,
    };
    Some(answer)
}
