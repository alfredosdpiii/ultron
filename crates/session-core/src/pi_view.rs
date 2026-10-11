//! `nativeSessionEntriesToPi` (ultron/migration.ts) and the Pi session the RPC views read: the format-4 session's
//! entries as Pi entries, in storage (parent-first) order, with the `main` tip mapped to a Pi leaf. Imported entries
//! keep their Pi originals (`ultron.pi.original`); branch-local copies (`ultron.pi.copyOf`) are not exported.

use serde_json::{Map, Value, json};
use session_file::{SessionManager, SystemEnv, js};
use session_journal::Session;
use session_journal::context::entry_context_messages;
use std::collections::HashMap;

const PI_ENTRY_CUSTOM_TYPE_PREFIX: &str = "pi-session:";
const PI_OMITTED_MESSAGE_CUSTOM_TYPE: &str = "pi-import:omitted-message";
const PI_ORIGINAL: &str = "ultron.pi.original";
const PI_COPY_OF: &str = "ultron.pi.copyOf";

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn with_base(base: &Map<String, Value>, rest: Vec<(&str, Value)>) -> Value {
    let mut out = base.clone();
    for (key, value) in rest {
        out.insert(key.into(), value);
    }
    Value::Object(out)
}

/// `nativeToPiEntry` for every entry type but compaction.
fn native_to_pi_entry(entry: &Value, base: &Map<String, Value>) -> Value {
    let optional = |key: &'static str| entry.get(key).filter(|v| !v.is_null()).map(|v| (key, v.clone()));
    match str_of(entry, "type") {
        Some("message") => {
            let message = &entry["message"];
            if str_of(message, "role") == Some("custom") {
                let mut rest = vec![
                    ("type", json!("custom_message")),
                    ("customType", message["customType"].clone()),
                    ("content", message["content"].clone()),
                    ("display", message["display"].clone()),
                ];
                rest.extend(message.get("details").map(|d| ("details", d.clone())));
                return with_base(base, rest);
            }
            with_base(base, vec![("type", json!("message")), ("message", message.clone())])
        }
        Some("branch_summary") => {
            let mut rest = vec![
                ("type", json!("branch_summary")),
                (
                    "fromId",
                    entry
                        .get("fromId")
                        .filter(|v| !v.is_null())
                        .cloned()
                        .unwrap_or(json!("root")),
                ),
                ("summary", entry["summary"].clone()),
            ];
            rest.extend(optional("details"));
            rest.extend(optional("usage"));
            rest.push(("fromHook", entry["fromHook"].clone()));
            with_base(base, rest)
        }
        Some("custom") => {
            let custom_type = str_of(entry, "customType").unwrap_or_default();
            if let Some(kind) = custom_type.strip_prefix(PI_ENTRY_CUSTOM_TYPE_PREFIX) {
                let mut out = entry["data"].as_object().cloned().unwrap_or_default();
                out.extend(base.clone());
                out.insert("type".into(), kind.into());
                return Value::Object(out);
            }
            if custom_type == PI_OMITTED_MESSAGE_CUSTOM_TYPE
                && let Some(message) = entry["data"].get("message")
            {
                let mut as_message = entry.clone();
                as_message["type"] = json!("message");
                as_message["message"] = message.clone();
                return native_to_pi_entry(&as_message, base);
            }
            let mut rest = vec![("type", json!("custom")), ("customType", json!(custom_type))];
            rest.extend(optional("data"));
            with_base(base, rest)
        }
        _ => with_base(base, vec![("type", entry["type"].clone())]),
    }
}

/// `findFirstKeptEntryId`: the Pi entry whose context messages through the branch's end are the retained tail.
fn first_kept_entry_id(previous: &[Value], tail: &[Value]) -> Option<String> {
    if tail.is_empty() {
        return None;
    }
    let mut collected: Vec<Value> = Vec::new();
    for entry in previous.iter().rev() {
        if str_of(entry, "type") == Some("compaction") {
            continue;
        }
        if str_of(entry, "type") == Some("message") && str_of(&entry["message"], "role") == Some("system") {
            continue;
        }
        let mut messages = pi_entry_context_messages(entry);
        messages.append(&mut collected);
        collected = messages;
        if collected.len() == tail.len() {
            return (collected == tail).then(|| str_of(entry, "id").unwrap_or_default().to_string());
        }
        if collected.len() > tail.len() {
            return None;
        }
    }
    None
}

/// Pi's `sessionEntryToContextMessages` for the Pi entries the conversion produces.
fn pi_entry_context_messages(entry: &Value) -> Vec<Value> {
    match str_of(entry, "type") {
        Some("custom_message") => vec![json!({
            "role": "custom", "customType": entry["customType"], "content": entry["content"],
            "display": entry["display"], "details": entry.get("details").cloned().unwrap_or(Value::Null),
            "timestamp": js::date_value(entry.get("timestamp")).map_or(Value::Null, Value::from),
        })],
        Some("message") => vec![entry["message"].clone()],
        _ => entry_context_messages(entry),
    }
}

/// `compactionToPi`: the compaction, plus its retained tail replayed when Pi cannot reference it.
fn compaction_to_pi(entry: &Value, base: &Map<String, Value>, branch: &[Value]) -> Vec<Value> {
    let tail = entry["retainedTail"].as_array().cloned().unwrap_or_default();
    let first_kept = first_kept_entry_id(branch, &tail);
    let id = str_of(entry, "id").unwrap_or_default().to_string();
    let mut rest = vec![
        ("type", json!("compaction")),
        ("summary", entry["summary"].clone()),
        (
            "firstKeptEntryId",
            json!(first_kept.clone().unwrap_or_else(|| id.clone())),
        ),
        ("tokensBefore", entry["tokensBefore"].clone()),
    ];
    rest.extend(
        entry
            .get("details")
            .filter(|v| !v.is_null())
            .map(|v| ("details", v.clone())),
    );
    rest.extend(
        entry
            .get("usage")
            .filter(|v| !v.is_null())
            .map(|v| ("usage", v.clone())),
    );
    rest.push(("fromHook", entry["fromHook"].clone()));
    let mut out = vec![with_base(base, rest)];
    if first_kept.is_none() {
        let mut parent = id.clone();
        for (index, message) in tail.iter().enumerate() {
            let tail_id = format!("{id}-tail-{index}");
            out.push(json!({"id": tail_id, "parentId": parent, "timestamp": base["timestamp"], "type": "message", "message": message}));
            parent = tail_id;
        }
    }
    out
}

/// A format-4 session's tree as Pi entries, and the Pi id of the `main` tip.
pub fn native_entries_to_pi(session: &Session) -> (Vec<Value>, Option<String>) {
    let state = session.state();
    let natives = state.entries();
    let original: HashMap<String, Value> = state
        .scan_values(PI_ORIGINAL, "")
        .into_iter()
        .map(|v| (v.key.clone(), v.value.clone()))
        .collect();
    let copy_of: HashMap<String, String> = state
        .scan_values(PI_COPY_OF, "")
        .into_iter()
        .filter_map(|v| v.value.as_str().map(|s| (v.key.clone(), s.to_string())))
        .collect();
    let original_id = |id: &str| copy_of.get(id).cloned().unwrap_or_else(|| id.to_string());
    let by_id: HashMap<&str, &Value> = natives.iter().filter_map(|e| Some((str_of(e, "id")?, e))).collect();
    let mut produced: HashMap<String, Vec<Value>> = HashMap::new();
    let pi_id_of = |produced: &HashMap<String, Vec<Value>>, id: Option<&str>| -> Option<String> {
        let id = original_id(id?);
        Some(
            produced
                .get(&id)
                .and_then(|items| items.last())
                .and_then(|e| str_of(e, "id"))
                .map_or(id, str::to_string),
        )
    };
    for entry in natives {
        let Some(id) = str_of(entry, "id") else { continue };
        if copy_of.contains_key(id) {
            continue;
        }
        let mut base = Map::new();
        base.insert("id".into(), id.into());
        base.insert(
            "parentId".into(),
            pi_id_of(&produced, str_of(entry, "parentId")).map_or(Value::Null, Value::from),
        );
        let timestamp = entry["timestamp"].as_i64().unwrap_or(0);
        base.insert("timestamp".into(), js::iso_string(timestamp).into());
        let items = if let Some(original) = original.get(id) {
            vec![original.clone()]
        } else if str_of(entry, "type") == Some("compaction") {
            // The Pi entries of the compaction's branch, root first.
            let mut ids = Vec::new();
            let mut at = str_of(entry, "parentId");
            while let Some(parent) = at {
                ids.push(parent.to_string());
                at = by_id.get(parent).and_then(|e| str_of(e, "parentId"));
            }
            let branch: Vec<Value> = ids
                .iter()
                .rev()
                .flat_map(|id| produced.get(&original_id(id)).cloned().unwrap_or_default())
                .collect();
            compaction_to_pi(entry, &base, &branch)
        } else {
            vec![native_to_pi_entry(entry, &base)]
        };
        produced.insert(id.to_string(), items);
    }
    let entries = natives
        .iter()
        .filter_map(|e| str_of(e, "id"))
        .flat_map(|id| produced.get(id).cloned().unwrap_or_default())
        .collect();
    let tip = session.branch_tip("main").flatten();
    (entries, pi_id_of(&produced, tip.as_deref()))
}

/// `PiSessionView`'s manager: the converted entries in an in-memory Pi session, its leaf at the `main` tip.
pub fn pi_manager(session: &Session) -> SessionManager {
    let (entries, leaf) = native_entries_to_pi(session);
    let cwd = session.metadata().cwd.clone();
    let header = json!({
        "type": "session", "version": 3, "id": session.id(), "timestamp": js::iso_string(0), "cwd": cwd,
    });
    let mut manager = SessionManager::in_memory(
        Box::new(SystemEnv),
        &cwd,
        None,
        Some(std::iter::once(header).chain(entries).collect()),
    )
    .or_else(|_| SessionManager::in_memory(Box::new(SystemEnv), &cwd, None, None))
    .expect("an empty in-memory session");
    match leaf {
        Some(leaf) => {
            let _ = manager.branch(&leaf);
        }
        None => manager.reset_leaf(),
    }
    manager
}
