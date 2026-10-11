//! `ctx.*` (port of the request side of `ultron/context-control.ts`) and the context-edit semantics it relies on
//! (`applyContextEdits` / `contextEditsOf` of `packages/agent/src/harness/session/context.ts`).
//!
//! The model edits what it sees, never the durable transcript: every change is an entry appended to the calling
//! lane's conversation branch (a `context_edit` custom entry for forget/summarize, its own custom entries for pins
//! and notes), and only that branch sees it. Guard rails: the current user message, a tool call still waiting for
//! its result, and pinned items are never edited; entries before the newest compaction are out of reach. A summary
//! moves tool calls and their results together. Entries appended while a turn runs are not on the branch until the
//! turn's next boundary; this module keeps them as pending so later reads in the same cell already see them.
//!
//! The branch itself lives in the session/harness: [`ContextBranches`] is what this module reads and appends.
//! Entries are the TS `Entry` JSON (`{id, parentId, seq, timestamp, type, ...}`).

use crate::util::{Clock, fields, js_trim, lock, stringify, system_clock, utf16_len, utf16_prefix};
use crate::{ApiModule, Request};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

pub const CONTEXT_EDIT_CUSTOM_TYPE: &str = "context_edit";
pub const CONTEXT_OMITTED_CUSTOM_TYPE: &str = "context_edit:omitted";
pub const CONTEXT_OMITTED_TOOL_RESULT_TEXT: &str = "[Tool result removed from context.]";
pub const CONTEXT_NOTE_CUSTOM_TYPE: &str = "ultron:context-note";
pub const CONTEXT_PIN_CUSTOM_TYPE: &str = "ultron:context-pin";

const HISTORY_DEFAULT: i64 = 20;
const HISTORY_MAX: i64 = 200;
const PREVIEW_CHARS: usize = 160;
const NOTE_MAX_CHARS: usize = 4000;
const SUMMARY_MAX_CHARS: usize = 8000;
const REASON_MAX_CHARS: usize = 500;
const COLLAPSE_HEAD_CHARS: usize = 200;
const COLLAPSE_MAX_TASKS: usize = 8;
const TRIM_HEAD_CHARS: usize = 4096;
const TRIM_TAIL_CHARS: usize = 1024;
const STATE_LIST_MAX: usize = 20;
const KINDS: &[&str] = &["user", "assistant", "tool", "custom", "note", "summary", "compaction"];
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The conversation branches of the session's lanes, as the harness keeps them.
pub trait ContextBranches: Send + Sync {
    /// The lane's committed branch, oldest first.
    fn entries(&self, lane: &str) -> Result<Vec<Value>, String>;
    /// Appends a custom entry (at once, or at the running turn's next boundary); returns its id.
    fn append_custom(&self, lane: &str, custom_type: &str, data: &Value) -> Result<String, String>;
    /// Id of the branch's newest committed entry.
    fn tip_id(&self, lane: &str) -> Result<Option<String>, String>;
}

// --- context edits (agent-core context.ts) ---------------------------------------------------------------------

/// One edit: `replacement` is `None` to omit the target, else `Some(content)` (a string or a content array).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextEdit {
    pub target_id: String,
    pub replacement: Option<Value>,
}

fn parse_context_edit(value: &Value) -> Option<ContextEdit> {
    let target = value.get("targetId")?.as_str().filter(|id| !id.is_empty())?;
    match value.get("replacement")? {
        Value::Null => Some(ContextEdit {
            target_id: target.into(),
            replacement: None,
        }),
        Value::Object(replacement) => {
            let content = replacement.get("content").filter(|c| c.is_string() || c.is_array())?;
            Some(ContextEdit {
                target_id: target.into(),
                replacement: Some(content.clone()),
            })
        }
        _ => None,
    }
}

/// The edits one entry carries; none for any other entry or malformed data.
pub fn context_edits_of(entry: &Value) -> Vec<ContextEdit> {
    if entry["type"] != "custom" || entry["customType"] != CONTEXT_EDIT_CUSTOM_TYPE || !entry["data"].is_object() {
        return Vec::new();
    }
    let data = &entry["data"];
    match data.get("edits").and_then(Value::as_array) {
        Some(list) => list.iter().filter_map(parse_context_edit).collect(),
        None => parse_context_edit(data).into_iter().collect(),
    }
}

fn content_call_ids(content: &Value) -> Vec<String> {
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part["type"] == "toolCall")
                .filter_map(|part| part["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn replace_content(message: &Value, content: &Value) -> Value {
    let role = message["role"].as_str().unwrap_or("");
    if !matches!(role, "user" | "assistant" | "toolResult" | "custom") {
        return message.clone();
    }
    let next = match content {
        Value::String(text) if role == "assistant" || role == "toolResult" => json!([{"type": "text", "text": text}]),
        other => other.clone(),
    };
    let mut message = message.clone();
    message["content"] = next;
    message
}

fn omitted_entry(entry: &Value) -> Value {
    json!({"id": entry["id"], "parentId": entry["parentId"], "seq": entry["seq"], "timestamp": entry["timestamp"],
        "type": "custom", "customType": CONTEXT_OMITTED_CUSTOM_TYPE})
}

/// An entry as the model sees it, and whether an edit produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub entry: Value,
    pub changed: bool,
}

/// `applyContextEdits`: applies the edits found among `entries` to them. Tool calls and results stay paired:
/// results of an omitted assistant message (or of calls a replacement dropped) are omitted with it, and an omitted
/// result whose call stays visible becomes a stub result. Later edits win.
pub fn apply_context_edits(entries: &[Value]) -> Vec<Applied> {
    let mut edits: Vec<(String, Option<Value>)> = Vec::new();
    for entry in entries {
        for edit in context_edits_of(entry) {
            match edits.iter_mut().find(|(id, _)| *id == edit.target_id) {
                Some(slot) => slot.1 = edit.replacement,
                None => edits.push((edit.target_id, edit.replacement)),
            }
        }
    }
    let unchanged = |entry: &Value| Applied {
        entry: entry.clone(),
        changed: false,
    };
    if edits.is_empty() {
        return entries.iter().map(unchanged).collect();
    }
    let edit_of = |id: &Value| {
        id.as_str()
            .and_then(|id| edits.iter().find(|(target, _)| target == id))
            .map(|e| &e.1)
    };
    let mut hidden_calls = HashSet::new();
    for entry in entries {
        if entry["type"] != "message" || entry["message"]["role"] != "assistant" {
            continue;
        }
        let Some(replacement) = edit_of(&entry["id"]) else {
            continue;
        };
        let kept: HashSet<String> = replacement
            .as_ref()
            .map(content_call_ids)
            .unwrap_or_default()
            .into_iter()
            .collect();
        for id in content_call_ids(&entry["message"]["content"]) {
            if !kept.contains(&id) {
                hidden_calls.insert(id);
            }
        }
    }
    entries
        .iter()
        .map(|entry| {
            let changed = |entry: Value| Applied { entry, changed: true };
            if entry["type"] == "custom" {
                return match edit_of(&entry["id"]) {
                    Some(None) => changed(omitted_entry(entry)),
                    _ => unchanged(entry),
                };
            }
            if entry["type"] != "message" {
                return unchanged(entry);
            }
            let message = &entry["message"];
            if message["role"] == "toolResult"
                && message["toolCallId"]
                    .as_str()
                    .is_some_and(|id| hidden_calls.contains(id))
            {
                return changed(omitted_entry(entry));
            }
            match edit_of(&entry["id"]) {
                None => unchanged(entry),
                Some(None) if message["role"] == "toolResult" => {
                    let mut next = entry.clone();
                    next["message"] = replace_content(message, &json!(CONTEXT_OMITTED_TOOL_RESULT_TEXT));
                    changed(next)
                }
                Some(None) => changed(omitted_entry(entry)),
                Some(Some(content)) => {
                    let mut next = entry.clone();
                    next["message"] = replace_content(message, content);
                    changed(next)
                }
            }
        })
        .collect()
}

// --- text helpers -------------------------------------------------------------------------------------------------

/// Readable text of a message, for previews and kept items (`messageText`).
pub fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => return text.clone(),
        Some(Value::Array(parts)) => {
            let texts: Vec<String> = parts
                .iter()
                .map(|part| {
                    if !part.is_object() {
                        return String::new();
                    }
                    match part["type"].as_str() {
                        Some("text") => part["text"].as_str().unwrap_or_default().to_string(),
                        Some("thinking") => part["thinking"].as_str().unwrap_or_default().to_string(),
                        Some("toolCall") => {
                            let name = match &part["name"] {
                                Value::String(name) => name.clone(),
                                Value::Null if part.get("name").is_none() => "undefined".into(),
                                other => stringify(other),
                            };
                            let arguments = match part.get("arguments") {
                                Some(value) if !value.is_null() => stringify(value),
                                _ => "{}".into(),
                            };
                            format!("{name}({})", utf16_prefix(&arguments, 2000))
                        }
                        Some("image") => "[image]".into(),
                        _ => String::new(),
                    }
                })
                .filter(|text| !text.is_empty())
                .collect();
            return texts.join("\n");
        }
        _ => {}
    }
    for key in ["summary", "output"] {
        if let Some(Value::String(text)) = message.get(key) {
            return text.clone();
        }
    }
    String::new()
}

fn js_space(ch: char) -> bool {
    ch.is_whitespace() || ch == '\u{feff}'
}

/// `oneLine`: whitespace runs collapsed to one space, trimmed, cut to `max` UTF-16 units with an ellipsis.
fn one_line(text: &str, max: usize) -> String {
    let mut flat = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if js_space(ch) {
            space = true;
        } else {
            if space {
                flat.push(' ');
                space = false;
            }
            flat.push(ch);
        }
    }
    if space {
        flat.push(' ');
    }
    let flat = js_trim(&flat);
    if utf16_len(flat) > max {
        format!("{}…", utf16_prefix(flat, max - 1))
    } else {
        flat.to_string()
    }
}

fn note_text(entry: &Value) -> String {
    if entry["type"] == "custom"
        && let Some(text) = entry["data"].get("text").and_then(Value::as_str)
    {
        return text.to_string();
    }
    String::new()
}

fn entry_bytes(entry: &Value) -> usize {
    match entry["type"].as_str() {
        Some("message") => {
            let message = &entry["message"];
            match message.get("content") {
                Some(content) if !content.is_null() => stringify(content).len(),
                _ => stringify(&Value::String(message_text(message))).len(),
            }
        }
        Some("compaction" | "branch_summary") => entry["summary"].as_str().map_or(0, str::len),
        Some("custom") if entry["customType"] == CONTEXT_NOTE_CUSTOM_TYPE => note_text(entry).len(),
        _ => 0,
    }
}

fn kind_of(entry: &Value) -> Option<&'static str> {
    match entry["type"].as_str()? {
        "message" => Some(match entry["message"]["role"].as_str() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            Some("toolResult") => "tool",
            _ => "custom",
        }),
        "compaction" => Some("compaction"),
        "branch_summary" => Some("summary"),
        "custom" if entry["customType"] == CONTEXT_NOTE_CUSTOM_TYPE => Some("note"),
        _ => None,
    }
}

fn entry_text(entry: &Value) -> String {
    match entry["type"].as_str() {
        Some("message") => message_text(&entry["message"]),
        Some("compaction" | "branch_summary") => entry["summary"].as_str().unwrap_or_default().to_string(),
        _ => note_text(entry),
    }
}

fn tool_call_ids(message: &Value) -> Vec<String> {
    if message["role"] != "assistant" {
        return Vec::new();
    }
    content_call_ids(&message["content"])
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, b)| match index {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// A finished task as it stands in the root's context after collapse on return.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLine {
    pub id: String,
    pub definition: String,
    pub key: String,
    pub status: String,
    pub cost: Option<f64>,
}

/// `taskLine`: one line that stands for a finished task.
pub fn task_line(task: &TaskLine) -> String {
    let key = if is_uuid(&task.key) {
        String::new()
    } else {
        format!(" key={}", task.key)
    };
    let cost = match task.cost {
        None => "cost=?".to_string(),
        Some(cost) => format!("cost=${cost:.4}"),
    };
    format!(
        "↳ task {} {}{key} {} {cost} · full result: await agents.result(\"{}\")",
        task.id, task.definition, task.status, task.id
    )
}

/// `collapsedCellText`: the collapsed form of a cell output that returned finished tasks.
pub fn collapsed_cell_text(entry_id: &str, original: &str, tasks: &[TaskLine]) -> String {
    let mut lines = vec![
        format!(
            "[collapsed on return: this cell output was {} bytes; ctx.get(\"{entry_id}\") shows it]",
            original.len()
        ),
        one_line(original, COLLAPSE_HEAD_CHARS),
    ];
    lines.extend(tasks.iter().take(COLLAPSE_MAX_TASKS).map(task_line));
    if tasks.len() > COLLAPSE_MAX_TASKS {
        lines.push(format!("↳ +{} more finished tasks", tasks.len() - COLLAPSE_MAX_TASKS));
    }
    lines.join("\n")
}

/// `trimmedResultText`: head and tail of a tool result trimmed before compaction (UTF-16 offsets).
pub fn trimmed_result_text(entry_id: &str, original: &str) -> String {
    let units = utf16_len(original);
    let cut = units as i64 - TRIM_HEAD_CHARS as i64 - TRIM_TAIL_CHARS as i64;
    let head = utf16_prefix(original, TRIM_HEAD_CHARS);
    let tail_start = utf16_prefix(original, units.saturating_sub(TRIM_TAIL_CHARS)).len();
    format!(
        "{head}\n[... {cut} characters trimmed before compaction; ctx.get(\"{entry_id}\") shows them ...]\n{}",
        &original[tail_start..]
    )
}

// --- the module -----------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct EditInfo {
    replacement: Option<Value>,
    source: String,
    reason: Option<String>,
}

/// A consistent read of one lane's branch plus this module's entries still pending behind a running turn.
struct BranchView {
    path: Vec<Value>,
    compaction_index: Option<usize>,
    window: Vec<Value>,
    applied: Vec<Applied>,
    /// Edit per target, in first-edit order (a later edit of the same target replaces it in place).
    edits: Vec<(String, EditInfo)>,
    /// Pinned ids, in pin order.
    pins: Vec<String>,
    protected: HashSet<String>,
}

impl BranchView {
    fn by_id(&self, id: &str) -> Option<&Value> {
        self.window.iter().find(|entry| entry["id"] == id)
    }
    fn applied(&self, id: &str) -> Option<&Applied> {
        self.applied.iter().find(|applied| applied.entry["id"] == id)
    }
    fn edit(&self, id: &str) -> Option<&EditInfo> {
        self.edits.iter().find(|(target, _)| target == id).map(|(_, info)| info)
    }
    fn pinned(&self, id: &str) -> bool {
        self.pins.iter().any(|pin| pin == id)
    }
}

fn is_omitted(entry: &Value) -> bool {
    entry["type"] == "custom" && entry["customType"] == CONTEXT_OMITTED_CUSTOM_TYPE
}

fn nonempty(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !js_trim(text).is_empty() => Ok(text.to_string()),
        _ => Err(format!("{name} must be a nonempty string")),
    }
}

fn id_list(value: Option<&Value>) -> Result<Vec<String>, String> {
    let items = value
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or("ids must be a nonempty list of item ids")?;
    let ids = items
        .iter()
        .map(|id| nonempty(Some(id), "id"))
        .collect::<Result<Vec<_>, _>>()?;
    if ids.len() > HISTORY_MAX as usize {
        return Err(format!("at most {HISTORY_MAX} ids per edit"));
    }
    let mut seen = HashSet::new();
    Ok(ids.into_iter().filter(|id| seen.insert(id.clone())).collect())
}

/// Collapse-on-return bookkeeping and the lanes this module serves.
#[derive(Default)]
struct Tracking {
    /// Root cell running now (tool call id).
    current_cell: Option<String>,
    /// Finished root-level tasks waiting for a cell to be attributed to.
    unattributed: Vec<TaskLine>,
    /// Cells (tool call ids, oldest first) whose output returned finished tasks and is not collapsed yet.
    cells: Vec<(String, Vec<TaskLine>)>,
    /// Per lane: tokens freed by trims since its newest assistant message (whose usage predates them).
    trimmed: Vec<(String, String, i64)>,
    /// Lanes that used ctx, and the root: only these carry notes and pins across compaction.
    lanes: HashSet<String>,
}

pub struct ContextControl {
    branches: Arc<dyn ContextBranches>,
    clock: Clock,
    trim: bool,
    /// (lane, entry) appended by this module that the branch does not show yet.
    pending: Mutex<Vec<(String, Value)>>,
    tracking: Mutex<Tracking>,
}

/// Whether old tool results are trimmed before a threshold compaction: ULTRON_COMPACTION_TRIM=off disables it.
pub fn compaction_trim_enabled(value: Option<&str>) -> bool {
    let raw = value.map(|value| js_trim(value).to_lowercase());
    !matches!(raw.as_deref(), Some("off" | "0" | "false"))
}

impl ContextControl {
    pub fn new(branches: Arc<dyn ContextBranches>) -> Self {
        let tracking = Tracking {
            lanes: HashSet::from(["main".to_string()]),
            ..Tracking::default()
        };
        Self {
            branches,
            clock: system_clock(),
            trim: compaction_trim_enabled(std::env::var("ULTRON_COMPACTION_TRIM").ok().as_deref()),
            pending: Mutex::new(Vec::new()),
            tracking: Mutex::new(tracking),
        }
    }

    /// Trim old tool results before a threshold compaction (default: [`compaction_trim_enabled`] of the env).
    pub fn with_trim(mut self, trim: bool) -> Self {
        self.trim = trim;
        self
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn known(&self, lane: &str) {
        lock(&self.tracking).lanes.insert(lane.to_string());
    }

    fn append(&self, lane: &str, custom_type: &str, data: Value) -> Result<String, String> {
        let id = self.branches.append_custom(lane, custom_type, &data)?;
        // While a turn runs the entry waits for the next boundary; later reads in this cell must already see it.
        if self.branches.tip_id(lane)?.as_deref() != Some(id.as_str()) {
            lock(&self.pending).push((
                lane.to_string(),
                json!({"id": id, "parentId": null, "seq": MAX_SAFE_INTEGER, "timestamp": (self.clock)(),
                    "type": "custom", "customType": custom_type, "data": data}),
            ));
        }
        Ok(id)
    }

    fn view(&self, lane: &str) -> Result<BranchView, String> {
        let committed = self.branches.entries(lane)?;
        let committed_ids: HashSet<&str> = committed.iter().filter_map(|entry| entry["id"].as_str()).collect();
        let mut pending = lock(&self.pending);
        pending.retain(|(_, entry)| !entry["id"].as_str().is_some_and(|id| committed_ids.contains(id)));
        let mut path = committed.clone();
        path.extend(
            pending
                .iter()
                .filter(|(owner, _)| owner == lane)
                .map(|(_, entry)| entry.clone()),
        );
        drop(pending);
        let compaction_index = path.iter().rposition(|entry| entry["type"] == "compaction");
        let window: Vec<Value> = path[compaction_index.map_or(0, |index| index + 1)..].to_vec();
        let applied = apply_context_edits(&window);
        let mut edits: Vec<(String, EditInfo)> = Vec::new();
        for entry in &window {
            let data = if entry["type"] == "custom" && entry["data"].is_object() {
                &entry["data"]
            } else {
                &Value::Null
            };
            for edit in context_edits_of(entry) {
                let info = EditInfo {
                    replacement: edit.replacement,
                    source: data["source"].as_str().unwrap_or("unknown").to_string(),
                    reason: data["reason"].as_str().map(str::to_string),
                };
                match edits.iter_mut().find(|(target, _)| *target == edit.target_id) {
                    Some(slot) => slot.1 = info,
                    None => edits.push((edit.target_id, info)),
                }
            }
        }
        let mut pins: Vec<String> = Vec::new();
        for entry in &path {
            if entry["type"] != "custom" || entry["customType"] != CONTEXT_PIN_CUSTOM_TYPE || !entry["data"].is_object()
            {
                continue;
            }
            let Some(target) = entry["data"]["targetId"].as_str() else {
                continue;
            };
            pins.retain(|pin| pin != target);
            if entry["data"]["pinned"] != false {
                pins.push(target.to_string());
            }
        }
        // The current user message, and a tool call still waiting for its result, are never edited.
        let mut protected = HashSet::new();
        if let Some(user) = window
            .iter()
            .rev()
            .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
        {
            protected.insert(user["id"].as_str().unwrap_or_default().to_string());
        }
        let results: HashSet<&str> = window
            .iter()
            .filter(|entry| entry["type"] == "message" && entry["message"]["role"] == "toolResult")
            .filter_map(|entry| entry["message"]["toolCallId"].as_str())
            .collect();
        for entry in &window {
            if entry["type"] == "message"
                && tool_call_ids(&entry["message"])
                    .iter()
                    .any(|id| !results.contains(id.as_str()))
            {
                protected.insert(entry["id"].as_str().unwrap_or_default().to_string());
            }
        }
        Ok(BranchView {
            path,
            compaction_index,
            window,
            applied,
            edits,
            pins,
            protected,
        })
    }

    fn item(view: &BranchView, entry: &Value) -> Option<Value> {
        let kind = kind_of(entry)?;
        let id = entry["id"].as_str().unwrap_or_default();
        let visible = view.applied(id);
        let omitted = visible.is_some_and(|applied| is_omitted(&applied.entry));
        let own = view.edit(id);
        let mut state = "visible";
        if own.is_some() || omitted || visible.is_none_or(|applied| applied.changed) {
            state = match own {
                Some(own) if own.source == "collapse" => "collapsed",
                Some(own) if own.source == "ctx.summarize" => "summarized",
                _ if omitted || own.is_some_and(|own| own.replacement.is_none()) => "forgotten",
                _ => "replaced",
            };
        }
        let mut item = Map::new();
        item.insert("id".into(), entry["id"].clone());
        item.insert("kind".into(), kind.into());
        if entry["type"] == "message"
            && entry["message"]["role"] == "toolResult"
            && let Some(tool) = entry["message"].get("toolName")
        {
            item.insert("tool".into(), tool.clone());
        }
        let bytes = match visible {
            Some(applied) if !omitted => entry_bytes(&applied.entry),
            _ => 0,
        };
        item.insert("bytes".into(), bytes.into());
        item.insert("original_bytes".into(), entry_bytes(entry).into());
        item.insert("preview".into(), one_line(&entry_text(entry), PREVIEW_CHARS).into());
        item.insert("state".into(), state.into());
        item.insert("pinned".into(), view.pinned(id).into());
        item.insert("protected".into(), view.protected.contains(id).into());
        item.insert("at".into(), entry["timestamp"].clone());
        Some(Value::Object(item))
    }

    fn visible_bytes(view: &BranchView) -> usize {
        view.applied
            .iter()
            .filter(|applied| !is_omitted(&applied.entry))
            .map(|applied| entry_bytes(&applied.entry))
            .sum()
    }

    /// Refuses the whole request when any id is unknown, outside the context, protected, or pinned.
    fn guard(view: &BranchView, ids: &[String]) -> Result<(), String> {
        let mut problems = Vec::new();
        for id in ids {
            match view.by_id(id) {
                None => problems.push(format!(
                    "{id} is not in the model's context on this branch (unknown, or before the last compaction)"
                )),
                Some(entry) if matches!(kind_of(entry), None | Some("compaction" | "summary")) => {
                    problems.push(format!(
                        "{id} is a {} entry and cannot be edited",
                        entry["type"].as_str().unwrap_or("undefined")
                    ))
                }
                Some(_) if view.protected.contains(id) => {
                    problems.push(format!("{id} belongs to the current user turn and cannot be edited"))
                }
                Some(_) if view.pinned(id) => problems.push(format!("{id} is pinned (ctx.unpin it first)")),
                Some(_) => {}
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!("Context edit refused: {}", problems.join("; ")))
        }
    }

    /// Tool calls and their results move together in a summarized span.
    fn closure(view: &BranchView, ids: &[String]) -> Vec<String> {
        let mut out: Vec<String> = ids.to_vec();
        let add = |out: &mut Vec<String>, id: &str| {
            if !out.iter().any(|known| known == id) {
                out.push(id.to_string());
            }
        };
        let mut call_owner: Vec<(String, String)> = Vec::new();
        let mut results_of: Vec<(String, Vec<String>)> = Vec::new();
        for entry in &view.window {
            if entry["type"] != "message" {
                continue;
            }
            let id = entry["id"].as_str().unwrap_or_default().to_string();
            for call in tool_call_ids(&entry["message"]) {
                match call_owner.iter_mut().find(|(known, _)| *known == call) {
                    Some(slot) => slot.1 = id.clone(),
                    None => call_owner.push((call, id.clone())),
                }
            }
            if entry["message"]["role"] == "toolResult" {
                let call = entry["message"]["toolCallId"].as_str().unwrap_or_default();
                if let Some((_, owner)) = call_owner.iter().find(|(known, _)| known == call) {
                    match results_of.iter_mut().find(|(known, _)| known == owner) {
                        Some(slot) => slot.1.push(id.clone()),
                        None => results_of.push((owner.clone(), vec![id.clone()])),
                    }
                }
            }
        }
        for id in ids {
            let Some(entry) = view.by_id(id) else { continue };
            if entry["type"] == "message" && entry["message"]["role"] == "toolResult" {
                let call = entry["message"]["toolCallId"].as_str().unwrap_or_default();
                if let Some((_, owner)) = call_owner.iter().find(|(known, _)| known == call) {
                    add(&mut out, owner);
                }
            }
        }
        for id in out.clone() {
            if let Some((_, results)) = results_of.iter().find(|(owner, _)| *owner == id) {
                for result in results {
                    add(&mut out, result);
                }
            }
        }
        out
    }

    /// Forgotten, pinned and noted items of the root branch (`ctx.state`, the `/rlm` panel).
    pub fn state(&self) -> Result<Value, String> {
        let view = self.view("main")?;
        let item = |id: &str| match view.by_id(id) {
            Some(entry) => json!({"id": id, "kind": kind_of(entry).unwrap_or("custom"),
                "preview": one_line(&entry_text(entry), 80)}),
            None => json!({"id": id}),
        };
        let edited: Vec<&(String, EditInfo)> = view.edits.iter().filter(|(id, _)| view.by_id(id).is_some()).collect();
        let forgotten: Vec<Value> = edited
            .iter()
            .filter(|(_, info)| info.source != "collapse" && info.source != "trim")
            .map(|(id, info)| {
                let mut value = item(id);
                value["source"] = info.source.clone().into();
                if let Some(reason) = &info.reason {
                    value["reason"] = reason.clone().into();
                }
                value
            })
            .collect();
        let last = |items: Vec<Value>| -> Vec<Value> {
            let skip = items.len().saturating_sub(STATE_LIST_MAX);
            items.into_iter().skip(skip).collect()
        };
        let pinned: Vec<Value> = view.pins.iter().map(|id| item(id)).collect();
        let notes = view
            .path
            .iter()
            .filter(|entry| entry["type"] == "custom" && entry["customType"] == CONTEXT_NOTE_CUSTOM_TYPE)
            .count();
        Ok(json!({
            "forgotten": last(forgotten.clone()),
            "forgottenCount": forgotten.len(),
            "collapsedCount": edited.len() - forgotten.len(),
            "pinned": last(pinned),
            "pinnedCount": view.pins.len(),
            "notes": notes,
        }))
    }

    pub fn handle_request(&self, kind: &str, payload: &Map<String, Value>, lane: &str) -> Result<Value, String> {
        if kind == "ctx.state" {
            fields(payload, &[])?;
            return self.state();
        }
        self.known(lane);
        let view = self.view(lane)?;
        match kind {
            "ctx.history" => {
                fields(payload, &["limit", "kinds"])?;
                let limit = match payload.get("limit") {
                    None => HISTORY_DEFAULT,
                    Some(value) => crate::util::safe_integer(value)
                        .filter(|limit| (1..=HISTORY_MAX).contains(limit))
                        .ok_or_else(|| format!("limit must be an integer between 1 and {HISTORY_MAX}"))?,
                };
                let kinds: Option<HashSet<&str>> = match payload.get("kinds") {
                    None | Some(Value::Null) => None,
                    Some(Value::Array(items))
                        if items.iter().all(|k| k.as_str().is_some_and(|k| KINDS.contains(&k))) =>
                    {
                        Some(items.iter().filter_map(Value::as_str).collect())
                    }
                    Some(_) => return Err(format!("kinds must be a list of: {}", KINDS.join(", "))),
                };
                let items: Vec<Value> = view
                    .window
                    .iter()
                    .filter_map(|entry| Self::item(&view, entry))
                    .filter(|item| {
                        kinds
                            .as_ref()
                            .is_none_or(|kinds| kinds.contains(item["kind"].as_str().unwrap_or("")))
                    })
                    .collect();
                let visible: u64 = items.iter().filter_map(|item| item["bytes"].as_u64()).sum();
                let shown: Vec<Value> = items[items.len().saturating_sub(limit as usize)..].to_vec();
                Ok(json!({"items": shown, "total": items.len(), "visible_bytes": visible,
                    "compacted": view.compaction_index.is_some()}))
            }
            "ctx.get" => {
                fields(payload, &["id"])?;
                let id = nonempty(payload.get("id"), "id")?;
                let entry = view.by_id(&id);
                let item = entry.and_then(|entry| Self::item(&view, entry));
                let (Some(entry), Some(mut item)) = (entry, item) else {
                    return Err(format!("Unknown context item: {id}"));
                };
                item["text"] = entry_text(entry).into();
                if entry["type"] == "message" {
                    item["message"] = entry["message"].clone();
                }
                item["visible_text"] = match view.applied(&id) {
                    Some(applied) if item["state"] != "forgotten" => entry_text(&applied.entry).into(),
                    _ => Value::Null,
                };
                Ok(item)
            }
            "ctx.forget" => {
                fields(payload, &["ids", "reason"])?;
                let ids = id_list(payload.get("ids"))?;
                let reason = utf16_prefix(&nonempty(payload.get("reason"), "reason")?, REASON_MAX_CHARS).to_string();
                Self::guard(&view, &ids)?;
                let fresh: Vec<String> = ids
                    .iter()
                    .filter(|id| view.edit(id).is_none_or(|info| info.replacement.is_some()))
                    .cloned()
                    .collect();
                let before = Self::visible_bytes(&view);
                if fresh.is_empty() {
                    return Ok(json!({"forgotten": [], "already": ids, "edit_id": null, "freed_bytes": 0}));
                }
                let edits: Vec<Value> = fresh
                    .iter()
                    .map(|id| json!({"targetId": id, "replacement": null}))
                    .collect();
                let edit_id = self.append(
                    lane,
                    CONTEXT_EDIT_CUSTOM_TYPE,
                    json!({"edits": edits, "source": "ctx.forget", "reason": reason}),
                )?;
                let after = Self::visible_bytes(&self.view(lane)?);
                let already: Vec<&String> = ids.iter().filter(|id| !fresh.contains(id)).collect();
                Ok(json!({"forgotten": fresh, "already": already, "edit_id": edit_id,
                    "freed_bytes": before.saturating_sub(after)}))
            }
            "ctx.summarize" => {
                fields(payload, &["ids", "text"])?;
                let text = nonempty(payload.get("text"), "text")?;
                if utf16_len(&text) > SUMMARY_MAX_CHARS {
                    return Err(format!("text exceeds {SUMMARY_MAX_CHARS} characters"));
                }
                let mut ids = Self::closure(&view, &id_list(payload.get("ids"))?);
                Self::guard(&view, &ids)?;
                let order = |id: &String| view.window.iter().position(|entry| entry["id"] == id.as_str());
                ids.sort_by_key(order);
                let first = ids[0].clone();
                let plural = if ids.len() == 1 { "" } else { "s" };
                let mut edits = vec![json!({"targetId": first, "replacement": {
                    "content": format!("[Summary of {} earlier item{plural} (ctx.summarize)]\n{text}", ids.len())}})];
                edits.extend(ids[1..].iter().map(|id| json!({"targetId": id, "replacement": null})));
                let before = Self::visible_bytes(&view);
                let edit_id = self.append(
                    lane,
                    CONTEXT_EDIT_CUSTOM_TYPE,
                    json!({"edits": edits, "source": "ctx.summarize"}),
                )?;
                let after = Self::visible_bytes(&self.view(lane)?);
                Ok(json!({"summarized": ids, "summary_id": first, "edit_id": edit_id,
                    "freed_bytes": before.saturating_sub(after)}))
            }
            "ctx.pin" | "ctx.unpin" => {
                fields(payload, &["id"])?;
                let id = nonempty(payload.get("id"), "id")?;
                if !view.by_id(&id).is_some_and(|entry| kind_of(entry).is_some()) {
                    return Err(format!("Unknown context item: {id}"));
                }
                let pin = kind == "ctx.pin";
                if view.pinned(&id) == pin {
                    return Ok(json!({"id": id, "pinned": pin, "changed": false}));
                }
                self.append(lane, CONTEXT_PIN_CUSTOM_TYPE, json!({"targetId": id, "pinned": pin}))?;
                Ok(json!({"id": id, "pinned": pin, "changed": true}))
            }
            "ctx.note" => {
                fields(payload, &["text"])?;
                let text = nonempty(payload.get("text"), "text")?;
                if utf16_len(&text) > NOTE_MAX_CHARS {
                    return Err(format!("note exceeds {NOTE_MAX_CHARS} characters"));
                }
                Ok(json!({"id": self.append(lane, CONTEXT_NOTE_CUSTOM_TYPE, json!({"text": text}))?}))
            }
            _ => Err(format!("Ultron RLM host request is not wired: {kind}")),
        }
    }
}

impl ApiModule for ContextControl {
    fn prefixes(&self) -> &[&str] {
        &["ctx."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        self.handle_request(request.kind, request.payload, request.lane)
    }
}

// --- harness hooks: collapse on return, trim before compaction, kept across compaction, observation ------------------

const COLLAPSE_MIN_BYTES: usize = 600;
const MAX_PENDING_CELLS: usize = 64;
const TRIM_MIN_CHARS: usize = 8192;
const TRIM_TARGET: f64 = 0.9;
const KEPT_ITEM_CHARS: usize = 4000;
const KEPT_TOTAL_CHARS: i64 = 16000;
pub const CONTEXT_KEPT_MESSAGE_TYPE: &str = "ultron-context-kept";

impl ContextControl {
    /// Harness `tool_start`: a root `rlm` cell starts; finished tasks not yet attributed belong to it.
    pub fn tool_start(&self, lane: &str, tool_name: &str, tool_call_id: &str) {
        if lane != "main" || tool_name != "rlm" {
            return;
        }
        let mut tracking = lock(&self.tracking);
        tracking.current_cell = Some(tool_call_id.to_string());
        if !tracking.unattributed.is_empty() {
            let lines = std::mem::take(&mut tracking.unattributed);
            set_cell(&mut tracking.cells, tool_call_id, lines);
        }
    }

    /// Harness `tool_end`.
    pub fn tool_end(&self, lane: &str, tool_call_id: &str) {
        let mut tracking = lock(&self.tracking);
        if lane == "main" && tracking.current_cell.as_deref() == Some(tool_call_id) {
            tracking.current_cell = None;
        }
    }

    /// Harness `message_end`: once the root model answered after a cell's output, collapse it.
    pub fn message_end(&self, lane: &str, role: &str) -> Result<(), String> {
        if lane == "main" && role == "assistant" && !lock(&self.tracking).cells.is_empty() {
            self.collapse()?;
        }
        Ok(())
    }

    /// Harness `run_end`.
    pub fn run_end(&self, lane: &str) -> Result<(), String> {
        if lane == "main" && !lock(&self.tracking).cells.is_empty() {
            self.collapse()?;
        }
        Ok(())
    }

    /// A task's terminal result is durable (`NativeTask` JSON). Root-level tasks are attributed to the root cell
    /// that returns them.
    pub fn task_ended(&self, task: &Value, cost: Option<f64>) {
        if task.get("parentId").is_some() {
            return;
        }
        let text = |key: &str| task[key].as_str().unwrap_or_default().to_string();
        let status = task["result"]["status"]
            .as_str()
            .map_or_else(|| text("state"), str::to_string);
        let line = TaskLine {
            id: text("id"),
            definition: text("definition"),
            key: text("key"),
            status,
            cost,
        };
        let mut tracking = lock(&self.tracking);
        match tracking.current_cell.clone() {
            Some(cell) => {
                let mut lines = tracking
                    .cells
                    .iter()
                    .find(|(id, _)| *id == cell)
                    .map(|(_, lines)| lines.clone())
                    .unwrap_or_default();
                lines.push(line);
                set_cell(&mut tracking.cells, &cell, lines);
            }
            None => {
                tracking.unattributed.push(line);
                if tracking.unattributed.len() > MAX_PENDING_CELLS {
                    tracking.unattributed.remove(0);
                }
            }
        }
        while tracking.cells.len() > MAX_PENDING_CELLS {
            tracking.cells.remove(0);
        }
    }

    /// Harness `entry_added`: drops a pending copy of the entry; for a context edit, the event observers receive.
    pub fn entry_added(&self, lane: &str, entry: &Value) -> Option<Value> {
        if entry["type"] != "custom" {
            return None;
        }
        lock(&self.pending).retain(|(_, pending)| pending["id"] != entry["id"]);
        if entry["customType"] != CONTEXT_EDIT_CUSTOM_TYPE {
            return None;
        }
        let data = if entry["data"].is_object() {
            &entry["data"]
        } else {
            &Value::Null
        };
        let mut event = Map::new();
        event.insert("lane".into(), lane.into());
        event.insert("editId".into(), entry["id"].clone());
        event.insert("source".into(), data["source"].as_str().unwrap_or("unknown").into());
        if let Some(reason) = data["reason"].as_str() {
            event.insert("reason".into(), reason.into());
        }
        let edits: Vec<Value> = context_edits_of(entry)
            .into_iter()
            .map(|edit| {
                let action = if edit.replacement.is_none() { "omit" } else { "replace" };
                json!({"targetId": edit.target_id, "action": action})
            })
            .collect();
        event.insert("edits".into(), edits.into());
        if let Some(tasks) = data["tasks"].as_array() {
            let tasks: Vec<Value> = tasks
                .iter()
                .map(|task| match task {
                    Value::String(text) => Value::String(text.clone()),
                    other => Value::String(stringify(other)),
                })
                .collect();
            event.insert("tasks".into(), tasks.into());
        }
        Some(Value::Object(event))
    }

    /// Collapse cell outputs the model has already answered after.
    fn collapse(&self) -> Result<(), String> {
        if lock(&self.tracking).cells.is_empty() {
            return Ok(());
        }
        let view = self.view("main")?;
        let mut edits = Vec::new();
        let mut tasks = Vec::new();
        let mut answered_after = false;
        for entry in view.window.iter().rev() {
            if entry["type"] != "message" {
                continue;
            }
            let message = &entry["message"];
            if message["role"] == "assistant" {
                answered_after = true;
                continue;
            }
            if message["role"] != "toolResult" || !answered_after {
                continue;
            }
            let call = message["toolCallId"].as_str().unwrap_or_default();
            let lines = {
                let mut tracking = lock(&self.tracking);
                let Some(index) = tracking.cells.iter().position(|(id, _)| id == call) else {
                    continue;
                };
                tracking.cells.remove(index).1
            };
            let id = entry["id"].as_str().unwrap_or_default();
            if view.pinned(id) || view.edit(id).is_some() {
                continue;
            }
            let original = message_text(message);
            if original.len() <= COLLAPSE_MIN_BYTES {
                continue;
            }
            let content = collapsed_cell_text(id, &original, &lines);
            if utf16_len(&content) >= utf16_len(&original) {
                continue;
            }
            edits.push(json!({"targetId": id, "replacement": {"content": content}}));
            tasks.extend(lines.into_iter().map(|line| line.id));
        }
        if edits.is_empty() {
            return Ok(());
        }
        edits.reverse();
        self.append(
            "main",
            CONTEXT_EDIT_CUSTOM_TYPE,
            json!({"edits": edits, "source": "collapse", "reason": "task results returned", "tasks": tasks}),
        )?;
        Ok(())
    }

    /// Harness `before_compaction`: for a threshold compaction, trims tool results the model answered after when
    /// that brings the estimate under 90% of the threshold; true declines the compaction.
    pub fn before_compaction(
        &self,
        lane: &str,
        reason: &str,
        tokens_before: i64,
        context_window: Option<i64>,
        reserve_tokens: i64,
    ) -> Result<bool, String> {
        if !self.trim || reason != "threshold" {
            return Ok(false);
        }
        self.known(lane);
        let Some(window) = context_window else {
            return Ok(false);
        };
        self.trim_instead(lane, tokens_before, window - reserve_tokens)
    }

    fn trim_instead(&self, lane: &str, tokens: i64, threshold: i64) -> Result<bool, String> {
        let view = self.view(lane)?;
        let target = threshold as f64 * TRIM_TARGET;
        let last_assistant = view
            .window
            .iter()
            .rev()
            .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
            .and_then(|entry| entry["id"].as_str().map(str::to_string));
        let credit = lock(&self.tracking)
            .trimmed
            .iter()
            .find(|(name, assistant, _)| name == lane && Some(assistant) == last_assistant.as_ref())
            .map_or(0, |(_, _, saved)| *saved);
        if (tokens - credit) as f64 <= target {
            return Ok(true);
        }
        let mut edits = Vec::new();
        let mut saved = 0i64;
        let mut answered_after = false;
        for entry in view.window.iter().rev() {
            if entry["type"] != "message" {
                continue;
            }
            let message = &entry["message"];
            if message["role"] == "assistant" {
                answered_after = true;
                continue;
            }
            if message["role"] != "toolResult" || !answered_after {
                continue;
            }
            let id = entry["id"].as_str().unwrap_or_default();
            if view.pinned(id) || view.edit(id).is_some() || view.protected.contains(id) {
                continue;
            }
            // Text only: an image's cost is not in its characters.
            if !message["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().all(|part| part["type"] == "text"))
            {
                continue;
            }
            let original = message_text(message);
            if utf16_len(&original) <= TRIM_MIN_CHARS {
                continue;
            }
            let content = trimmed_result_text(id, &original);
            saved += (utf16_len(&original) as i64 - utf16_len(&content) as i64).div_euclid(4);
            edits.push(json!({"targetId": id, "replacement": {"content": content}}));
        }
        let Some(last_assistant) = last_assistant else {
            return Ok(false);
        };
        if edits.is_empty() || (tokens - credit - saved) as f64 > target {
            return Ok(false);
        }
        edits.reverse();
        self.append(
            lane,
            CONTEXT_EDIT_CUSTOM_TYPE,
            json!({"edits": edits, "source": "trim", "reason": "trimmed old tool results instead of compacting"}),
        )?;
        let mut tracking = lock(&self.tracking);
        tracking.trimmed.retain(|(name, _, _)| name != lane);
        tracking
            .trimmed
            .push((lane.to_string(), last_assistant, credit + saved));
        Ok(true)
    }

    /// Notes and pinned items that the newest compaction cut off, as one bounded text block.
    pub fn kept_across_compaction(&self, lane: &str) -> Result<Option<String>, String> {
        let view = self.view(lane)?;
        let Some(index) = view.compaction_index else {
            return Ok(None);
        };
        let retained: HashSet<String> = view.path[index]["retainedTail"]
            .as_array()
            .map(|tail| tail.iter().map(stringify).collect())
            .unwrap_or_default();
        let before = &view.path[..index];
        let omitted: HashSet<String> = before
            .iter()
            .flat_map(context_edits_of)
            .filter(|edit| edit.replacement.is_none())
            .map(|edit| edit.target_id)
            .collect();
        let mut lines: Vec<String> = Vec::new();
        let mut budget = KEPT_TOTAL_CHARS;
        let mut add = |line: String| {
            if budget <= 0 {
                return;
            }
            let bounded = if utf16_len(&line) as i64 > budget {
                format!("{}…", utf16_prefix(&line, budget as usize - 1))
            } else {
                line
            };
            budget -= utf16_len(&bounded) as i64;
            lines.push(bounded);
        };
        for entry in before {
            let id = entry["id"].as_str().unwrap_or_default();
            if entry["type"] == "custom" && entry["customType"] == CONTEXT_NOTE_CUSTOM_TYPE && !omitted.contains(id) {
                add(format!("- note {id}: {}", note_text(entry)));
            } else if view.pinned(id) && entry["type"] == "message" && !retained.contains(&stringify(&entry["message"]))
            {
                let text = message_text(&entry["message"]);
                let text = if utf16_len(&text) > KEPT_ITEM_CHARS {
                    format!("{}…", utf16_prefix(&text, KEPT_ITEM_CHARS - 1))
                } else {
                    text
                };
                add(format!(
                    "- pinned {id} ({}): {text}",
                    entry["message"]["role"].as_str().unwrap_or("undefined")
                ));
            }
        }
        if lines.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "Notes and pinned items kept across compaction (ctx.note / ctx.pin):\n{}",
            lines.join("\n")
        )))
    }

    /// Harness `transform_context`: after a compaction summary, shows again the notes and pinned items it cut off
    /// (only for lanes that used ctx, and the root). `None` leaves the messages unchanged.
    pub fn transform_context(&self, lane: &str, messages: &[Value]) -> Result<Option<Vec<Value>>, String> {
        let Some(index) = messages
            .iter()
            .position(|message| message["role"] == "compactionSummary")
        else {
            return Ok(None);
        };
        if !lock(&self.tracking).lanes.contains(lane) {
            return Ok(None);
        }
        let Some(kept) = self.kept_across_compaction(lane)? else {
            return Ok(None);
        };
        let mut out = messages.to_vec();
        out.insert(
            index + 1,
            json!({"role": "custom", "customType": CONTEXT_KEPT_MESSAGE_TYPE, "content": kept, "display": false,
                "timestamp": (self.clock)()}),
        );
        Ok(Some(out))
    }
}

/// Sets a cell's task lines, keeping its place in the insertion order (a JS Map).
fn set_cell(cells: &mut Vec<(String, Vec<TaskLine>)>, id: &str, lines: Vec<TaskLine>) {
    match cells.iter_mut().find(|(known, _)| known == id) {
        Some(slot) => slot.1 = lines,
        None => cells.push((id.to_string(), lines)),
    }
}
