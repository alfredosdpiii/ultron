//! Model context from session entries: the active path, compaction, context edits, and settings.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::js;

/// Map key for an id value, matching JavaScript `Map` lookups by primitive: `undefined` is its own key.
pub(crate) fn id_key(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_string(), js::stringify)
}

pub(crate) fn str_key(id: &str) -> String {
    js::stringify(&Value::from(id))
}

pub(crate) fn field<'a>(entry: &'a Value, name: &str) -> Option<&'a Value> {
    entry.get(name)
}

pub(crate) fn entry_type(entry: &Value) -> Option<&str> {
    entry.get("type").and_then(Value::as_str)
}

fn message_role(entry: &Value) -> Option<&str> {
    entry.get("message")?.get("role")?.as_str()
}

pub(crate) fn is_message_with_role(entry: &Value, role: &str) -> bool {
    entry_type(entry) == Some("message") && message_role(entry) == Some(role)
}

/// Builds a JSON object from `(key, value)` pairs, skipping `None` (JavaScript `undefined` fields).
pub(crate) fn object(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        if let Some(value) = value {
            map.insert(key.to_string(), value);
        }
    }
    Value::Object(map)
}

/// Sets `key` like a spread override `{ ...value, key }`: in place when present, else appended.
fn with_field(value: &Value, key: &str, new: Option<Value>) -> Value {
    let mut out = value.clone();
    if let Value::Object(map) = &mut out {
        match new {
            Some(new) => {
                map.insert(key.to_string(), new);
            }
            None => {
                map.shift_remove(key);
            }
        }
    }
    out
}

/// Which leaf a context walk starts from: the TypeScript `leafId` argument.
#[derive(Clone, Copy, Debug)]
pub enum Leaf<'a> {
    /// `undefined` (or an empty id): the last entry.
    Last,
    /// `null`: before the first entry, so the context is empty.
    Root,
    /// An entry id; an unknown id falls back to the last entry.
    Id(&'a Value),
}

/// Walks parent links from `leaf` to the root (at most `limit` steps); returns the path root-first.
pub(crate) fn walk_to_root<'a>(
    leaf: Option<&'a Value>,
    limit: usize,
    lookup: impl Fn(&str) -> Option<&'a Value>,
) -> Vec<&'a Value> {
    let mut path = Vec::new();
    let mut current = leaf;
    while let Some(entry) = current {
        // ponytail: upstream loops forever on a parent cycle; stop once the walk is longer than the index.
        if path.len() > limit {
            break;
        }
        path.push(entry);
        current = entry
            .get("parentId")
            .filter(|p| js::truthy(Some(p)))
            .and_then(|p| lookup(&id_key(Some(p))));
    }
    path.reverse();
    path
}

/// `buildSessionPath`: the entries from the root to the selected leaf.
pub(crate) fn session_path<'a>(entries: &[&'a Value], leaf: Leaf<'_>) -> Vec<&'a Value> {
    let by_id: HashMap<String, &Value> = entries.iter().map(|e| (id_key(e.get("id")), *e)).collect();
    let start = match leaf {
        Leaf::Root => return Vec::new(),
        Leaf::Id(id) if js::truthy(Some(id)) => by_id.get(&id_key(Some(id))).copied().or(entries.last().copied()),
        _ => entries.last().copied(),
    };
    walk_to_root(start, by_id.len(), |key| by_id.get(key).copied())
}

/// `buildContextEntries`: the active path with the latest compaction first, then the entries it
/// kept (from `firstKeptEntryId`, without system messages), then everything after it.
pub fn build_context_entries<'a>(entries: &'a [Value], leaf: Leaf<'_>) -> Vec<&'a Value> {
    context_entries(&entries.iter().collect::<Vec<_>>(), leaf)
}

pub(crate) fn context_entries<'a>(entries: &[&'a Value], leaf: Leaf<'_>) -> Vec<&'a Value> {
    context_entries_of_path(session_path(entries, leaf))
}

fn context_entries_of_path(path: Vec<&Value>) -> Vec<&Value> {
    let Some(compaction) = path.iter().rev().copied().find(|e| entry_type(e) == Some("compaction")) else {
        return path;
    };
    let Some(compaction_idx) = path.iter().position(|e| e.get("id") == compaction.get("id")) else {
        return path;
    };
    let first_kept = compaction.get("firstKeptEntryId");
    let mut out = vec![compaction];
    let mut found = false;
    for entry in &path[..compaction_idx] {
        if entry.get("id") == first_kept {
            found = true;
        }
        if found && !is_message_with_role(entry, "system") {
            out.push(entry);
        }
    }
    out.extend(&path[compaction_idx + 1..]);
    out
}

fn custom_message(entry: &Value) -> Value {
    let content = entry
        .get("content")
        .filter(|c| !c.is_null())
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    object(vec![
        ("role", Some("custom".into())),
        ("customType", entry.get("customType").cloned()),
        ("content", Some(content)),
        ("display", entry.get("display").cloned()),
        ("details", entry.get("details").cloned()),
        ("timestamp", Some(js::date_json(entry.get("timestamp")))),
    ])
}

/// `sessionEntryToContextMessages`: the messages one entry contributes to model context.
pub fn entry_messages(entry: &Value) -> Vec<Value> {
    match entry_type(entry) {
        Some("message") => {
            let Some(message) = entry.get("message") else {
                return Vec::new();
            };
            let role = message.get("role").and_then(Value::as_str);
            let empty_content = js::nullish(message.get("content"));
            match role {
                Some("system") if empty_content => vec![with_field(message, "content", Some("".into()))],
                Some("user" | "assistant" | "toolResult") if empty_content => {
                    vec![with_field(message, "content", Some(Value::Array(Vec::new())))]
                }
                _ => vec![message.clone()],
            }
        }
        Some("custom_message") => vec![custom_message(entry)],
        Some("branch_summary") if js::truthy(entry.get("summary")) => vec![object(vec![
            ("role", Some("branchSummary".into())),
            ("summary", entry.get("summary").cloned()),
            ("fromId", entry.get("fromId").cloned()),
            ("timestamp", Some(js::date_json(entry.get("timestamp")))),
        ])],
        Some("compaction") => {
            let summary = object(vec![
                ("role", Some("compactionSummary".into())),
                ("summary", entry.get("summary").cloned()),
                ("tokensBefore", entry.get("tokensBefore").cloned()),
                ("timestamp", Some(js::date_json(entry.get("timestamp")))),
            ]);
            match entry.get("systemMessage") {
                Some(system) if js::truthy(Some(system)) => vec![system.clone(), summary],
                _ => vec![summary],
            }
        }
        _ => Vec::new(),
    }
}

fn project_entry(entry: &Value, edit: Option<&Value>) -> Vec<Value> {
    let messages = entry_messages(entry);
    let Some(edit) = edit else { return messages };
    let replacement = match edit.get("replacement") {
        Some(Value::Null) => return Vec::new(),
        // Upstream throws on an edit without `replacement`; leave the target unedited.
        None => return messages,
        Some(replacement) => replacement,
    };
    messages
        .into_iter()
        .map(|message| {
            let role = message.get("role").and_then(Value::as_str);
            if !matches!(role, Some("user" | "assistant" | "toolResult" | "custom")) {
                return message;
            }
            let content = match replacement.get("content") {
                Some(Value::String(text)) if matches!(role, Some("assistant" | "toolResult")) => {
                    Some(serde_json::json!([{ "type": "text", "text": text }]))
                }
                other => other.cloned(),
            };
            with_field(&message, "content", content)
        })
        .collect()
}

/// One context entry and the messages it contributes after context edits.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedEntry<'a> {
    pub source: &'a Value,
    pub messages: Vec<Value>,
}

/// `SessionProjection`: provenance-preserving model context.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionProjection<'a> {
    pub entries: Vec<ProjectedEntry<'a>>,
    pub messages: Vec<Value>,
    /// The latest thinking level on the path (`"off"` by default; `None` when an entry left it undefined).
    pub thinking_level: Option<Value>,
    /// `{ provider, modelId }` from the latest model change or assistant message.
    pub model: Option<Value>,
}

/// `SessionContext`: what reaches the model.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<Value>,
    pub thinking_level: Option<Value>,
    pub model: Option<Value>,
}

fn settings(path: &[&Value]) -> (Option<Value>, Option<Value>) {
    let mut thinking = Some(Value::from("off"));
    let mut model = None;
    for entry in path {
        match entry_type(entry) {
            Some("thinking_level_change") => thinking = entry.get("thinkingLevel").cloned(),
            Some("model_change") => {
                model = Some(object(vec![
                    ("provider", entry.get("provider").cloned()),
                    ("modelId", entry.get("modelId").cloned()),
                ]));
            }
            Some("message") if message_role(entry) == Some("assistant") => {
                let message = &entry["message"];
                model = Some(object(vec![
                    ("provider", message.get("provider").cloned()),
                    ("modelId", message.get("model").cloned()),
                ]));
            }
            _ => {}
        }
    }
    (thinking, model)
}

/// `buildSessionProjection`.
pub fn build_session_projection<'a>(entries: &'a [Value], leaf: Leaf<'_>) -> SessionProjection<'a> {
    session_projection(&entries.iter().collect::<Vec<_>>(), leaf)
}

pub(crate) fn session_projection<'a>(entries: &[&'a Value], leaf: Leaf<'_>) -> SessionProjection<'a> {
    let path = session_path(entries, leaf);
    let (thinking_level, model) = settings(&path);
    let context = context_entries_of_path(path);
    let mut edits: HashMap<String, &Value> = HashMap::new();
    for entry in &context {
        if entry_type(entry) == Some("context_edit") {
            edits.insert(id_key(entry.get("targetId")), entry);
        }
    }
    let projected: Vec<ProjectedEntry> = context
        .into_iter()
        .enumerate()
        .map(|(i, source)| ProjectedEntry {
            source,
            // An older compaction can sit inside the newest one's kept range; only the newest
            // (at index 0) contributes its checkpoint and summary.
            messages: if entry_type(source) == Some("compaction") && i > 0 {
                Vec::new()
            } else {
                project_entry(source, edits.get(&id_key(source.get("id"))).copied())
            },
        })
        .collect();
    SessionProjection {
        messages: projected.iter().flat_map(|e| e.messages.iter().cloned()).collect(),
        entries: projected,
        thinking_level,
        model,
    }
}

/// `buildSessionContext`.
pub fn build_session_context(entries: &[Value], leaf: Leaf<'_>) -> SessionContext {
    build_session_projection(entries, leaf).into()
}

impl From<SessionProjection<'_>> for SessionContext {
    fn from(p: SessionProjection<'_>) -> Self {
        SessionContext {
            messages: p.messages,
            thinking_level: p.thinking_level,
            model: p.model,
        }
    }
}

/// `getLatestCompactionEntry`.
pub fn latest_compaction(entries: &[Value]) -> Option<&Value> {
    entries.iter().rev().find(|e| entry_type(e) == Some("compaction"))
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| js::join_string(b.get("text")))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn system_messages(messages: &[Value]) -> impl Iterator<Item = &Value> {
    messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
}

fn as_array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `getCurrentTools`: tools after replaying every system message's removals and additions.
pub fn current_tools(messages: &[Value]) -> Vec<Value> {
    let mut tools: IndexMap<String, &Value> = IndexMap::new();
    for message in system_messages(messages) {
        for tool in as_array(message.get("toolsRemoved")) {
            tools.shift_remove(&id_key(tool.get("name")));
        }
        for tool in as_array(message.get("toolsAdded")) {
            tools.insert(id_key(tool.get("name")), tool);
        }
    }
    tools.into_values().cloned().collect()
}

/// `getCurrentSystemMessage`: every system message replayed into one (content appended,
/// sections patched by name, tools resolved). `None` when there is no system state.
pub fn current_system_message(messages: &[Value]) -> Option<Value> {
    let mut content = Vec::new();
    let mut sections: IndexMap<String, Value> = IndexMap::new();
    let mut timestamp: Option<Value> = None;
    for message in system_messages(messages) {
        if timestamp.is_none() && !js::nullish(message.get("timestamp")) {
            timestamp = message.get("timestamp").cloned();
        }
        let text = content_text(message.get("content"));
        if !text.is_empty() {
            content.push(text);
        }
        if let Some(Value::Object(patch)) = message.get("sections") {
            for (name, value) in js::object_entries(patch) {
                if value.is_null() {
                    sections.shift_remove(name);
                } else {
                    sections.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let tools = current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(object(vec![
        ("role", Some("system".into())),
        ("content", Some(content.join("\n\n").into())),
        (
            "sections",
            (!sections.is_empty()).then(|| Value::Object(sections.into_iter().collect())),
        ),
        ("toolsAdded", (!tools.is_empty()).then_some(Value::Array(tools))),
        ("timestamp", Some(timestamp.unwrap_or_else(|| 0.into()))),
    ]))
}
