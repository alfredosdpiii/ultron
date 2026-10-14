//! Explicit, repeatable migration between Pi and Ultron sessions (`ultron/migration.ts`): [`import_pi_session`]
//! copies a whole Pi session tree into a new native format-4 session, [`export_native_session_to_pi`] writes a native
//! session's whole tree back as a Pi session (rollback). Nothing here reads Pi's data directory implicitly: every
//! source path is given by the caller.
//!
//! Pi entry mapping (Pi entry ids are kept as native entry ids, parents are unchanged):
//! - `message`, `compaction`, `branch_summary`, `custom` -> the native entry of the same type;
//! - `custom_message` -> a native `message` entry with a `custom` role message;
//! - every other Pi entry type -> a native `custom` entry `pi-session:<type>` whose data is the Pi entry without
//!   `type`/`id`/`parentId`/`timestamp`;
//! - at the end: the `main` tip is the Pi leaf, labels are Pi's resolved labels, the name is Pi's name, and the `main`
//!   lane configuration carries the model and thinking level Pi resolves at the leaf;
//! - a `context_edit` dominated by its target is baked into the target (replacement or omission); one that only some
//!   branches carry is applied per branch on copies (`<id>~<editId>`, recorded in `ultron.pi.copyOf`);
//! - Pi-reported usage becomes the usage ledger's imported historical root `import:pi:<piSessionId>`;
//! - an entry the native form cannot reproduce exactly keeps its Pi original in `ultron.pi.original`.
//!
//! The native file is byte-for-byte what the TypeScript product writes for the same Pi file
//! (`fixtures/migration/migration.json`).

use crate::pi_view::{native_session_as_pi, native_to_pi_entry};
use delegation::task_store::TaskStore;
use delegation::usage::{Measurement, UsageLedger, UsageOptions};
use serde_json::{Map, Value, json};
use session_file::{Leaf, SessionManager, SystemEnv, build_context_entries, build_session_context, entry_messages, js};
use session_journal::session::{BRANCH_TIP, ENTRY_LABEL, LANE_CONFIG, LANE_STATE, SESSION_NAME};
use session_journal::{Clock, SessionRepo, SharedClock, SystemClock, Write};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

pub const PI_ENTRY_CUSTOM_TYPE_PREFIX: &str = "pi-session:";
pub const PI_OMITTED_MESSAGE_CUSTOM_TYPE: &str = "pi-import:omitted-message";
pub const PI_LEAF_MARKER_CUSTOM_TYPE: &str = "ultron:pi-leaf";
pub const PI_ORIGINAL: &str = "ultron.pi.original";
pub const PI_COPY_OF: &str = "ultron.pi.copyOf";
const USAGE_NAMESPACE: &str = "ultron.usage";
const USAGE_KEY: &str = "root";
/// Pi's `CURRENT_SESSION_VERSION`.
const PI_SESSION_VERSION: u64 = 3;

const KNOWN_PI_ENTRY_TYPES: [&str; 11] = [
    "message",
    "custom_message",
    "compaction",
    "branch_summary",
    "custom",
    "model_change",
    "thinking_level_change",
    "label",
    "session_info",
    "usage",
    "context_edit",
];

/// `piImportUsageRoot`.
pub fn pi_import_usage_root(pi_session_id: &str) -> String {
    format!("import:pi:{pi_session_id}")
}

/// `nativeSessionIdForPiSession`: a UUID-shaped (version 8) digest of the Pi session id.
pub fn native_session_id_for_pi_session(pi_session_id: &str) -> String {
    let digest = sha256(format!("ultron:pi-import:{pi_session_id}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn sha256(data: &[u8]) -> Vec<u8> {
    let hex = kernel_apis::util::sha256_hex(data);
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0))
        .collect()
}

fn str_of<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str)
}

fn id_of(entry: &Value) -> &str {
    str_of(entry, "id").unwrap_or_default()
}

fn type_of(entry: &Value) -> &str {
    str_of(entry, "type").unwrap_or_default()
}

/// `parseTimestamp`: `Date.parse(value)`, else `fallback`.
fn parse_timestamp(value: Option<&Value>, fallback: i64) -> i64 {
    value
        .and_then(Value::as_str)
        .and_then(js::parse_date)
        .unwrap_or(fallback)
}

/// Copy `key` of `from` into `out` when it is present (JavaScript drops `undefined` fields).
fn put(out: &mut Map<String, Value>, key: &str, from: &Value) {
    if let Some(value) = from.get(key) {
        out.insert(key.into(), value.clone());
    }
}

/// `replaceContent`: Pi's content replacement for one projected message.
fn replace_content(message: &Value, content: Option<&Value>) -> Value {
    let role = str_of(message, "role");
    if !matches!(role, Some("user" | "assistant" | "toolResult" | "custom")) {
        return message.clone();
    }
    let next = match content {
        Some(Value::String(text)) if matches!(role, Some("assistant" | "toolResult")) => {
            Some(json!([{"type": "text", "text": text}]))
        }
        other => other.cloned(),
    };
    let mut out = message.clone();
    if let Value::Object(map) = &mut out {
        match next {
            Some(next) => {
                map.insert("content".into(), next);
            }
            None => {
                map.shift_remove("content");
            }
        }
    }
    out
}

/// `applyEdit`: `None` is no edit, `Some(Null)` an omission, `Some(replacement)` a replacement.
fn apply_edit(messages: Vec<Value>, edit: Option<&Value>) -> Vec<Value> {
    match edit {
        None => messages,
        Some(Value::Null) => Vec::new(),
        Some(replacement) => messages
            .iter()
            .map(|message| replace_content(message, replacement.get("content")))
            .collect(),
    }
}

/// Parent-first order that otherwise keeps file order (`PiTree`).
struct PiTree {
    by_id: HashMap<String, Value>,
    children: HashMap<String, Vec<String>>,
}

impl PiTree {
    fn new(entries: &[Value]) -> Self {
        let by_id: HashMap<String, Value> = entries.iter().map(|e| (id_of(e).to_string(), e.clone())).collect();
        let mut tree = PiTree {
            by_id,
            children: HashMap::new(),
        };
        for entry in entries {
            if let Some(parent) = tree.parent_of(entry) {
                tree.children.entry(parent).or_default().push(id_of(entry).to_string());
            }
        }
        tree
    }

    fn child_ids(&self, id: &str) -> &[String] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }

    /// The parent as Pi's tree resolves it: missing or self parents make the entry a root.
    fn parent_of(&self, entry: &Value) -> Option<String> {
        let parent = str_of(entry, "parentId")?;
        (parent != id_of(entry) && self.by_id.contains_key(parent)).then(|| parent.to_string())
    }

    /// Whether `descendant` is below `ancestor` and every leaf under `ancestor` passes through it.
    fn dominates(&self, ancestor: &str, descendant: &str) -> bool {
        let Some(mut current) = self.by_id.get(descendant) else {
            return false;
        };
        let mut steps = 0;
        while let Some(parent) = self.parent_of(current) {
            if self.child_ids(&parent).len() != 1 {
                return false;
            }
            if parent == ancestor {
                return true;
            }
            current = &self.by_id[&parent];
            steps += 1;
            if steps > self.by_id.len() {
                return false;
            }
        }
        false
    }

    fn insertion_order(&self, entries: &[Value]) -> Vec<Value> {
        let mut out = Vec::new();
        let mut placed: HashSet<String> = HashSet::new();
        let mut waiting: Vec<(String, Vec<Value>)> = Vec::new();
        fn take(waiting: &mut Vec<(String, Vec<Value>)>, id: &str) -> Option<Vec<Value>> {
            let at = waiting.iter().position(|(key, _)| key == id)?;
            Some(waiting.remove(at).1)
        }
        let place = |entry: &Value,
                     out: &mut Vec<Value>,
                     placed: &mut HashSet<String>,
                     waiting: &mut Vec<(String, Vec<Value>)>| {
            let mut stack = vec![entry.clone()];
            while let Some(next) = stack.pop() {
                let id = id_of(&next).to_string();
                out.push(next);
                placed.insert(id.clone());
                if let Some(mut children) = take(waiting, &id) {
                    children.reverse();
                    stack.extend(children);
                }
            }
        };
        for entry in entries {
            if placed.contains(id_of(entry)) {
                continue;
            }
            match self.parent_of(entry) {
                Some(parent) if !placed.contains(&parent) => match waiting.iter_mut().find(|(key, _)| *key == parent) {
                    Some((_, siblings)) => siblings.push(entry.clone()),
                    None => waiting.push((parent, vec![entry.clone()])),
                },
                _ => place(entry, &mut out, &mut placed, &mut waiting),
            }
        }
        // Only cycles remain; Pi cannot reach these entries from a root either.
        let rest: Vec<Value> = waiting.iter().flat_map(|(_, children)| children.clone()).collect();
        for entry in rest {
            if !placed.contains(id_of(&entry)) {
                place(&entry, &mut out, &mut placed, &mut waiting);
            }
        }
        out
    }
}

/// `remap`: point an edit's or label's `targetId` (a compaction's `firstKeptEntryId`) at its copy.
fn remap(entry: &mut Value, mapping: &HashMap<String, String>) {
    let kind = type_of(entry);
    if !matches!(kind, "context_edit" | "label" | "compaction") {
        return;
    }
    let field = if kind == "compaction" {
        "firstKeptEntryId"
    } else {
        "targetId"
    };
    if let Some(next) = str_of(entry, field).and_then(|target| mapping.get(target)) {
        entry[field] = json!(next);
    }
}

/// `splitBranchLocalEdits`: every context edit on a message on its own path ends up dominated by its target, the
/// path from the target to the last fork copied for the edit's branch. Returns the entries and `copyOf`.
fn split_branch_local_edits(input: &[Value]) -> (Vec<Value>, HashMap<String, String>) {
    let mut entries = input.to_vec();
    let mut copy_of: HashMap<String, String> = HashMap::new();
    let mut ids: HashSet<String> = entries.iter().map(|e| id_of(e).to_string()).collect();
    let mut split = |edit_id: &str, entries: &mut Vec<Value>| -> bool {
        let tree = PiTree::new(entries);
        let Some(edit) = tree.by_id.get(edit_id) else {
            return false;
        };
        let Some(target) = str_of(edit, "targetId").and_then(|t| tree.by_id.get(t)) else {
            return false;
        };
        if !matches!(type_of(target), "message" | "custom_message") || tree.dominates(id_of(target), edit_id) {
            return false;
        }
        // Path target .. parent(edit), oldest first; the edit must lie below its target.
        let mut path: Vec<&Value> = Vec::new();
        let mut parent_id = tree.parent_of(edit);
        while let Some(id) = parent_id {
            let parent = &tree.by_id[&id];
            path.insert(0, parent);
            if id_of(parent) == id_of(target) || path.len() > tree.by_id.len() {
                break;
            }
            parent_id = tree.parent_of(parent);
        }
        if path.first().map(|e| id_of(e)) != Some(id_of(target)) {
            return false;
        }
        let next = |index: usize| -> String {
            match path.get(index + 1) {
                Some(entry) => id_of(entry).to_string(),
                None => edit_id.to_string(),
            }
        };
        let mut fork = None;
        for (index, entry) in path.iter().enumerate() {
            if tree.child_ids(id_of(entry)).iter().any(|child| *child != next(index)) {
                fork = Some(index);
            }
        }
        let Some(fork) = fork else { return false };
        let mut mapping: HashMap<String, String> = HashMap::new();
        let mut copies: HashMap<String, Value> = HashMap::new();
        let mut parent = target.get("parentId").cloned().unwrap_or(Value::Null);
        let edit_base = copy_of.get(edit_id).cloned().unwrap_or_else(|| edit_id.to_string());
        for original in &path[..=fork] {
            let original_id = id_of(original).to_string();
            let source = copy_of
                .get(&original_id)
                .cloned()
                .unwrap_or_else(|| original_id.clone());
            let base = format!("{source}~{edit_base}");
            let mut id = base.clone();
            let mut counter = 2;
            while ids.contains(&id) {
                id = format!("{base}~{counter}");
                counter += 1;
            }
            ids.insert(id.clone());
            mapping.insert(original_id.clone(), id.clone());
            copy_of.insert(id.clone(), source);
            let mut copy = (*original).clone();
            copy["id"] = json!(id);
            copy["parentId"] = parent;
            copies.insert(original_id, copy);
            parent = json!(id);
        }
        for copy in copies.values_mut() {
            remap(copy, &mapping);
        }
        // Move the edit's branch onto the copies and redirect its references to them.
        let head = next(fork);
        let mut moved: HashSet<String> = HashSet::from([head.clone()]);
        let mut queue = VecDeque::from([head.clone()]);
        while let Some(id) = queue.pop_front() {
            for child in tree.child_ids(&id) {
                moved.insert(child.clone());
                queue.push_back(child.clone());
            }
        }
        let old = std::mem::take(entries);
        for mut entry in old {
            let id = id_of(&entry).to_string();
            if moved.contains(&id) {
                if id == head {
                    entry["parentId"] = parent.clone();
                }
                remap(&mut entry, &mapping);
            }
            entries.push(entry);
            // Each copy goes right after its original, so edit order (the later edit wins) is kept.
            if let Some(copy) = copies.remove(&id) {
                entries.push(copy);
            }
        }
        true
    };
    let mut budget = 64 * (input.len() + 1);
    let mut changed = true;
    while changed && budget > 0 {
        changed = false;
        let edits: Vec<String> = entries
            .iter()
            .filter(|e| type_of(e) == "context_edit")
            .map(|e| id_of(e).to_string())
            .collect();
        for edit in edits {
            if budget == 0 {
                continue;
            }
            if split(&edit, &mut entries) {
                changed = true;
                budget -= 1;
            }
        }
    }
    (entries, copy_of)
}

/// `piUsageEntries`: Pi-reported usage per entry, as Pi's session stats count it.
fn pi_usage_entries(entries: &[&Value]) -> Vec<(i64, Value)> {
    let mut out = Vec::new();
    for entry in entries {
        let usage = match type_of(entry) {
            "usage" | "compaction" | "branch_summary" => entry.get("usage"),
            "message" if matches!(str_of(&entry["message"], "role"), Some("assistant" | "toolResult")) => {
                entry["message"].get("usage")
            }
            _ => None,
        };
        if let Some(usage) = usage.filter(|u| !u.is_null()) {
            out.push((parse_timestamp(entry.get("timestamp"), 0), usage.clone()));
        }
    }
    out
}

struct ImportPlan {
    /// The replacement per message entry id, for edits that reach every leaf under the target (`None`: an edit
    /// without a replacement field).
    message_edits: HashMap<String, Option<Value>>,
    unapplied_context_edits: Vec<String>,
    tails: HashMap<String, Vec<Value>>,
}

fn plan_import(entries: &[Value], tree: &PiTree) -> ImportPlan {
    let edits: Vec<&Value> = entries.iter().filter(|e| type_of(e) == "context_edit").collect();
    let mut message_edits = HashMap::new();
    let mut unapplied = Vec::new();
    for edit in &edits {
        let target = str_of(edit, "targetId").and_then(|t| tree.by_id.get(t));
        let editable = target.is_some_and(|t| matches!(type_of(t), "message" | "custom_message"));
        // Edits appear in path order along a single chain, so the later edit wins as in Pi.
        match target {
            Some(target) if editable && tree.dominates(id_of(target), id_of(edit)) => {
                message_edits.insert(id_of(target).to_string(), edit.get("replacement").cloned());
            }
            _ => unapplied.push(id_of(edit).to_string()),
        }
    }
    let mut tails = HashMap::new();
    for compaction in entries.iter().filter(|e| type_of(e) == "compaction") {
        let id = json!(id_of(compaction));
        let context = build_context_entries(entries, Leaf::Id(&id));
        let mut replacements: HashMap<String, Option<Value>> = HashMap::new();
        for entry in &context {
            if type_of(entry) == "context_edit" {
                replacements.insert(
                    str_of(entry, "targetId").unwrap_or_default().to_string(),
                    entry.get("replacement").cloned(),
                );
            }
        }
        // Later edits of kept entries apply on every leaf below the compaction only when they dominate it.
        for edit in &edits {
            if tree.dominates(id_of(compaction), id_of(edit)) {
                replacements.insert(
                    str_of(edit, "targetId").unwrap_or_default().to_string(),
                    edit.get("replacement").cloned(),
                );
            }
        }
        let tail: Vec<Value> = context
            .iter()
            .skip(1)
            .flat_map(|entry| {
                if type_of(entry) == "compaction" {
                    return Vec::new();
                }
                let edit = replacements.get(id_of(entry)).cloned().flatten();
                apply_edit(entry_messages(entry), edit.as_ref())
            })
            .collect();
        tails.insert(id_of(compaction).to_string(), tail);
    }
    ImportPlan {
        message_edits,
        unapplied_context_edits: unapplied,
        tails,
    }
}

fn object(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

fn edited_message_body(message: &Value, edit: Option<&Value>) -> Map<String, Value> {
    match edit {
        None => object(vec![("type", json!("message")), ("message", message.clone())]),
        Some(Value::Null) => object(vec![
            ("type", json!("custom")),
            ("customType", json!(PI_OMITTED_MESSAGE_CUSTOM_TYPE)),
            ("data", json!({ "message": message })),
        ]),
        Some(replacement) => object(vec![
            ("type", json!("message")),
            ("message", replace_content(message, replacement.get("content"))),
        ]),
    }
}

/// `toNativeEntryBody`.
fn to_native_entry_body(entry: &Value, plan: &ImportPlan) -> Map<String, Value> {
    let edit = plan.message_edits.get(id_of(entry)).cloned().flatten();
    let mut out = Map::new();
    match type_of(entry) {
        "message" => return edited_message_body(&entry["message"], edit.as_ref()),
        "custom_message" => {
            // `createCustomMessage`.
            let mut message = Map::new();
            message.insert("role".into(), json!("custom"));
            put(&mut message, "customType", entry);
            put(&mut message, "content", entry);
            put(&mut message, "display", entry);
            put(&mut message, "details", entry);
            message.insert("timestamp".into(), js::date_json(entry.get("timestamp")));
            return edited_message_body(&Value::Object(message), edit.as_ref());
        }
        "compaction" => {
            out.insert("type".into(), json!("compaction"));
            put(&mut out, "summary", entry);
            let tail = plan.tails.get(id_of(entry)).cloned().unwrap_or_default();
            out.insert("retainedTail".into(), Value::Array(tail));
            put(&mut out, "tokensBefore", entry);
            put(&mut out, "details", entry);
            put(&mut out, "usage", entry);
            out.insert("fromHook".into(), from_hook(entry));
        }
        "branch_summary" => {
            out.insert("type".into(), json!("branch_summary"));
            // Pi encodes a root source as the "root" sentinel.
            match entry.get("fromId") {
                Some(Value::String(from)) if from == "root" => {
                    out.insert("fromId".into(), Value::Null);
                }
                _ => put(&mut out, "fromId", entry),
            }
            put(&mut out, "summary", entry);
            put(&mut out, "details", entry);
            put(&mut out, "usage", entry);
            out.insert("fromHook".into(), from_hook(entry));
        }
        "custom" => {
            out.insert("type".into(), json!("custom"));
            put(&mut out, "customType", entry);
            put(&mut out, "data", entry);
        }
        other => {
            let mut data = entry.as_object().cloned().unwrap_or_default();
            for key in ["type", "id", "parentId", "timestamp"] {
                data.shift_remove(key);
            }
            out.insert("type".into(), json!("custom"));
            out.insert(
                "customType".into(),
                json!(format!("{PI_ENTRY_CUSTOM_TYPE_PREFIX}{other}")),
            );
            out.insert("data".into(), Value::Object(data));
        }
    }
    out
}

/// `fromHook ?? false`.
fn from_hook(entry: &Value) -> Value {
    match entry.get("fromHook") {
        None | Some(Value::Null) => json!(false),
        Some(value) => value.clone(),
    }
}

/// `reproducesPiEntry`: whether export rebuilds exactly this Pi entry from its native entry.
fn reproduces_pi_entry(original: &Value, native: &Map<String, Value>, timestamp: i64) -> bool {
    if native.get("type").and_then(Value::as_str) == Some("compaction") {
        return false;
    }
    let mut entry = native.clone();
    entry.insert("seq".into(), json!(0));
    entry.insert("timestamp".into(), json!(timestamp));
    let mut base = Map::new();
    base.insert("id".into(), native.get("id").cloned().unwrap_or(Value::Null));
    base.insert(
        "parentId".into(),
        native.get("parentId").cloned().unwrap_or(Value::Null),
    );
    base.insert("timestamp".into(), json!(js::iso_string(timestamp)));
    native_to_pi_entry(&Value::Object(entry), &base) == *original
}

/// What an import wrote (`ImportPiSessionResult`).
#[derive(Clone, Debug, PartialEq)]
pub struct ImportResult {
    pub session_id: String,
    pub path: PathBuf,
    /// Native entries written for Pi entries (copies are not counted).
    pub imported: usize,
    /// Pi entries that could not be imported (always empty: every type is mapped or preserved).
    pub skipped: Vec<(String, usize)>,
    /// Unknown Pi entry types kept as `pi-session:<type>` custom entries, with counts, in first-seen order.
    pub preserved: Vec<(String, usize)>,
    pub unapplied_context_edits: Vec<String>,
    pub branch_copies: usize,
    /// `{root, entries, cost}` of the imported usage.
    pub usage_root: String,
    pub usage_entries: usize,
    pub usage_cost: f64,
}

impl ImportResult {
    /// The TypeScript result object.
    pub fn to_json(&self) -> Value {
        let counted = |items: &[(String, usize)]| -> Value {
            items
                .iter()
                .map(|(kind, count)| json!({"type": kind, "count": count}))
                .collect()
        };
        json!({
            "sessionId": self.session_id,
            "path": self.path.display().to_string(),
            "imported": self.imported,
            "skipped": counted(&self.skipped),
            "preserved": counted(&self.preserved),
            "unappliedContextEdits": self.unapplied_context_edits,
            "branchCopies": self.branch_copies,
            "importedUsage": {"root": self.usage_root, "entries": self.usage_entries,
                "cost": delegation::usage::number(self.usage_cost)},
        })
    }
}

/// Why an import failed.
#[derive(Clone, Debug, PartialEq)]
pub enum ImportError {
    /// `PiSessionAlreadyImportedError`: nothing was written; the native copy is `session_id` at `path`.
    AlreadyImported {
        pi_session_id: String,
        session_id: String,
        path: PathBuf,
    },
    Failed(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::AlreadyImported {
                pi_session_id,
                session_id,
                path,
            } => write!(
                f,
                "Pi session {pi_session_id} was already imported as native session {session_id}: {}",
                path.display()
            ),
            ImportError::Failed(message) => f.write_str(message),
        }
    }
}

/// The repository clock of an import: the Pi timestamps drive `now()` (ids come from the system generator).
struct PiClock {
    time: Arc<Mutex<i64>>,
}

impl Clock for PiClock {
    fn now(&mut self) -> i64 {
        *self.time.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn uuidv7(&mut self, at: i64) -> String {
        SystemClock.uuidv7(at)
    }
    fn next_id(&mut self) -> String {
        SystemClock.next_id()
    }
}

/// The usage ledger's store during an import: it starts empty and keeps what the ledger writes.
struct Captured(Arc<Mutex<Option<Value>>>);

impl TaskStore for Captured {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(None)
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(document.clone());
        Ok(())
    }
}

/// Called before each commit of an import with its 1-based number; an error fails that commit (the TypeScript tests
/// make `session.mutate` fail the same way).
pub type CommitHook<'a> = &'a mut dyn FnMut(usize) -> Result<(), String>;

/// `importPiSession`: the whole tree of the Pi session at `pi_session_path` as a new native session under
/// `sessions_root` (entry ids, parents and timestamps kept; the `main` tip is the Pi leaf). A failed import leaves
/// no session file behind.
pub fn import_pi_session(pi_session_path: &Path, sessions_root: &Path) -> Result<ImportResult, ImportError> {
    import_pi_session_with(pi_session_path, sessions_root, &mut |_| Ok(()))
}

/// [`import_pi_session`] with a hook before each commit.
pub fn import_pi_session_with(
    pi_session_path: &Path,
    sessions_root: &Path,
    hook: CommitHook<'_>,
) -> Result<ImportResult, ImportError> {
    let pi_path = PathBuf::from(session_journal::repo::absolute(&pi_session_path.display().to_string()));
    let shown = pi_path.display().to_string();
    let file_entries = session_file::load_entries_from_file(&shown).unwrap_or_default();
    let header = match file_entries.first() {
        Some(header) if type_of(header) == "session" => header.clone(),
        _ => {
            return Err(ImportError::Failed(format!(
                "Not a Pi session file (missing session header): {shown}"
            )));
        }
    };
    let cwd = str_of(&header, "cwd").unwrap_or_default().to_string();
    // In memory: Pi's format migrations apply without rewriting the source file.
    let manager = SessionManager::in_memory(Box::new(SystemEnv), &cwd, None, Some(file_entries))
        .map_err(|e| ImportError::Failed(e.to_string()))?;
    let pi_entries: Vec<Value> = manager.entries().into_iter().cloned().collect();
    let pi_session_id = str_of(&header, "id").unwrap_or_default().to_string();
    let session_id = native_session_id_for_pi_session(&pi_session_id);
    let (split, copy_of) = split_branch_local_edits(&pi_entries);
    let tree = PiTree::new(&split);
    let plan = plan_import(&split, &tree);
    let pi_by_id: HashMap<&str, &Value> = pi_entries.iter().map(|e| (id_of(e), e)).collect();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let time = Arc::new(Mutex::new(parse_timestamp(header.get("timestamp"), now)));
    let set_time = |at: i64| *time.lock().unwrap_or_else(PoisonError::into_inner) = at;
    let clock: SharedClock = Arc::new(Mutex::new(PiClock { time: time.clone() }));
    let repo = SessionRepo::new(sessions_root, clock);
    if let Some(existing) = repo.list(None).into_iter().find(|m| m.id == session_id) {
        return Err(ImportError::AlreadyImported {
            pi_session_id,
            session_id,
            path: existing.path,
        });
    }
    let mut session = repo
        .create(&cwd, Some(&session_id), None)
        .map_err(ImportError::Failed)?;
    let path = session.metadata().path.clone();
    let mut commits = 0;
    let mut commit = |session: &mut session_journal::Session, writes: &[Write]| -> Result<(), String> {
        commits += 1;
        hook(commits)?;
        session.commit(writes).map(drop)
    };
    let mut written = || -> Result<ImportResult, String> {
        let mut preserved: Vec<(String, usize)> = Vec::new();
        let mut imported = 0;
        let mut at = *time.lock().unwrap_or_else(PoisonError::into_inner);
        for entry in tree.insertion_order(&split) {
            let id = id_of(&entry).to_string();
            let copied = copy_of.get(&id);
            let kind = type_of(&entry).to_string();
            if copied.is_none() && !KNOWN_PI_ENTRY_TYPES.contains(&kind.as_str()) {
                match preserved.iter_mut().find(|(k, _)| *k == kind) {
                    Some((_, count)) => *count += 1,
                    None => preserved.push((kind, 1)),
                }
            }
            at = parse_timestamp(entry.get("timestamp"), at);
            set_time(at);
            let mut native = to_native_entry_body(&entry, &plan);
            native.insert("id".into(), json!(id));
            native.insert(
                "parentId".into(),
                tree.parent_of(&entry).map_or(Value::Null, Value::from),
            );
            let mut writes = vec![Write::Entry(native.clone())];
            match copied {
                Some(original) => writes.push(Write::set(PI_COPY_OF, &id, json!(original))),
                None => {
                    // Compare with the Pi entry as written: a split may have re-parented or re-targeted this one.
                    let original = pi_by_id.get(id.as_str()).copied().unwrap_or(&Value::Null);
                    if !reproduces_pi_entry(original, &native, at) {
                        writes.push(Write::set(PI_ORIGINAL, &id, original.clone()));
                    }
                    imported += 1;
                }
            }
            commit(&mut session, &writes)?;
        }

        // Usage Pi reported, as one imported historical root of the session's usage ledger.
        let usage_root = pi_import_usage_root(&pi_session_id);
        let refs: Vec<&Value> = pi_entries.iter().collect();
        let reported = pi_usage_entries(&refs);
        if !reported.is_empty() {
            let captured = Arc::new(Mutex::new(None));
            let ledger = UsageLedger::new(Box::new(Captured(captured.clone())), UsageOptions::default())?;
            let calls: Vec<(f64, Measurement, Option<String>)> = reported
                .iter()
                .map(|(at, usage)| {
                    let measured = Measurement {
                        input_tokens: usage["input"].as_f64(),
                        output_tokens: usage["output"].as_f64(),
                        total_tokens: usage["totalTokens"].as_f64(),
                        cost: usage["cost"]["total"].as_f64(),
                        wall_ms: Some(0.0),
                    };
                    (*at as f64, measured, None)
                })
                .collect();
            ledger.import_history(&usage_root, "pi", &calls)?;
            let document = captured.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(document) = document {
                commit(&mut session, &[Write::set(USAGE_NAMESPACE, USAGE_KEY, document)])?;
            }
        }

        // Derived state at the Pi leaf: tip, labels, name, and the lane configuration Pi would resume with.
        let leaf = manager
            .leaf_id()
            .and_then(Value::as_str)
            .filter(|leaf| tree.by_id.contains_key(*leaf));
        let mut writes = vec![Write::set(BRANCH_TIP, "main", leaf.map_or(Value::Null, Value::from))];
        for entry in &pi_entries {
            if let Some(label) = manager.label(id_of(entry)) {
                writes.push(Write::set(ENTRY_LABEL, id_of(entry), label.clone()));
            }
        }
        if let Some(name) = manager.session_name() {
            writes.push(Write::set(SESSION_NAME, "", json!(name)));
        }
        let context = manager.build_session_context();
        if let Some(model) = &context.model {
            let mut configuration = Map::new();
            configuration.insert(
                "model".into(),
                json!({"provider": model["provider"], "modelId": model["modelId"]}),
            );
            if let Some(level) = &context.thinking_level {
                configuration.insert("thinkingLevel".into(), level.clone());
            }
            // Tools are not recorded by Pi; the host activates its own tool set when it opens the lane.
            configuration.insert("activeToolNames".into(), json!([]));
            writes.push(Write::set(LANE_CONFIG, "main", Value::Object(configuration)));
            writes.push(Write::set(
                LANE_STATE,
                "main",
                json!({"currentOperationId": null, "lastOperationId": null, "inbox": []}),
            ));
        }
        commit(&mut session, &writes)?;
        let cost = reported.iter().fold(0.0, |total, (_, usage)| {
            total + usage["cost"]["total"].as_f64().unwrap_or(0.0)
        });
        Ok(ImportResult {
            session_id: session_id.clone(),
            path: path.clone(),
            imported,
            skipped: Vec::new(),
            preserved,
            unapplied_context_edits: plan.unapplied_context_edits.clone(),
            branch_copies: copy_of.len(),
            usage_root,
            usage_entries: reported.len(),
            usage_cost: cost,
        })
    };
    let result = written();
    drop(session);
    result.map_err(|error| {
        // Never leave a partial import behind: it would block a retry.
        let _ = std::fs::remove_file(&path);
        ImportError::Failed(error)
    })
}

/// What an export wrote (`ExportNativeSessionToPiResult`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExportResult {
    pub session_id: String,
    pub path: PathBuf,
    /// Pi entries written (without the header).
    pub entries: usize,
}

/// `piLabelsOf`: the labels Pi's label entries resolve to, in first-set order.
fn pi_labels_of(entries: &[Value]) -> Vec<(String, String)> {
    let mut labels: Vec<(String, String)> = Vec::new();
    for entry in entries.iter().filter(|e| type_of(e) == "label") {
        let target = str_of(entry, "targetId").unwrap_or_default().to_string();
        let label = entry
            .get("label")
            .filter(|l| js::truthy(Some(l)))
            .and_then(Value::as_str);
        match label {
            Some(label) => match labels.iter_mut().find(|(t, _)| *t == target) {
                Some((_, existing)) => *existing = label.to_string(),
                None => labels.push((target, label.to_string())),
            },
            None => labels.retain(|(t, _)| *t != target),
        }
    }
    labels
}

/// `piNameOf`: the last `session_info` entry's trimmed name.
fn pi_name_of(entries: &[Value]) -> Option<String> {
    let entry = entries.iter().rev().find(|e| type_of(e) == "session_info")?;
    entry
        .get("name")
        .and_then(Value::as_str)
        .map(js::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// `exportNativeSessionToPi`: the native session at `session_path` (in `<sessionsRoot>/<cwd-dir>/`) written whole
/// as a Pi session at `output_path`, which must not exist. Pi resumes at its last entry, the native `main` tip;
/// labels, the name, and the `main` lane's model and thinking level that differ from what the entries say are
/// appended after it.
pub fn export_native_session_to_pi(session_path: &Path, output_path: &Path) -> Result<ExportResult, String> {
    let path = PathBuf::from(session_journal::repo::absolute(&session_path.display().to_string()));
    let root = path.parent().and_then(Path::parent).unwrap_or(Path::new("/"));
    let repo = SessionRepo::new(root, SystemClock::shared());
    let metadata = repo
        .list(None)
        .into_iter()
        .find(|m| m.path == path)
        .ok_or_else(|| format!("Not a native session in its session directory: {}", path.display()))?;
    let session = repo.open(&metadata)?;
    let converted = native_session_as_pi(&session, "main");
    let state = session.state();
    let natives = state.entries();
    let native_ids: HashSet<&str> = natives.iter().map(id_of).collect();
    let mut entries = converted.entries;
    let mut ids: HashSet<String> = entries.iter().map(|e| id_of(e).to_string()).collect();
    let newest = natives
        .iter()
        .filter_map(|e| e["timestamp"].as_i64())
        .fold(metadata.created_at, i64::max);
    let mut stamp = json!(js::iso_string(newest));
    let mut leaf: Option<String> = entries.last().map(|e| id_of(e).to_string());
    let mut append = |entries: &mut Vec<Value>, leaf: &mut Option<String>, stamp: &Value, entry: Value, kind: &str| {
        let mut id = format!("ultron-{kind}");
        let mut counter = 1;
        while ids.contains(&id) {
            id = format!("ultron-{kind}-{counter}");
            counter += 1;
        }
        ids.insert(id.clone());
        let mut out = entry.as_object().cloned().unwrap_or_default();
        out.insert("id".into(), json!(id));
        out.insert("parentId".into(), leaf.clone().map_or(Value::Null, Value::from));
        out.insert("timestamp".into(), stamp.clone());
        entries.push(Value::Object(out));
        *leaf = Some(id);
    };

    // Pi resumes at its last entry: make that the native main tip.
    if let Some(tip) = &converted.tip_id
        && converted.leaf_id != leaf
    {
        if natives.iter().any(|e| str_of(e, "parentId") == Some(tip.as_str())) {
            leaf = converted.leaf_id.clone();
            append(
                &mut entries,
                &mut leaf,
                &stamp,
                json!({"type": "custom", "customType": PI_LEAF_MARKER_CUSTOM_TYPE}),
                "leaf",
            );
        } else {
            let items = converted.produced.get(tip).cloned().unwrap_or_default();
            let moved: HashSet<&str> = items.iter().map(id_of).collect();
            entries.retain(|e| !moved.contains(id_of(e)));
            entries.extend(items.iter().cloned());
            leaf = converted.leaf_id.clone();
        }
    }
    if let Some(found) = entries.iter().find(|e| Some(id_of(e)) == leaf.as_deref()) {
        stamp = found["timestamp"].clone();
    }

    // Labels set natively after import (or never recorded as Pi entries).
    let pi_labels = pi_labels_of(&entries);
    let native_labels: Vec<(String, String)> = state
        .scan_values(ENTRY_LABEL, "")
        .into_iter()
        .filter_map(|v| v.value.as_str().map(|l| (v.key.clone(), l.to_string())))
        .collect();
    let mut targets: Vec<String> = pi_labels.iter().map(|(t, _)| t.clone()).collect();
    for (target, _) in &native_labels {
        if !targets.contains(target) {
            targets.push(target.clone());
        }
    }
    for target in targets {
        let label = native_labels.iter().find(|(t, _)| *t == target).map(|(_, l)| l.clone());
        let pi = pi_labels.iter().find(|(t, _)| *t == target).map(|(_, l)| l.clone());
        if pi == label || !native_ids.contains(target.as_str()) {
            continue;
        }
        let mut entry = json!({"type": "label", "targetId": target});
        if let Some(label) = label {
            entry["label"] = json!(label);
        }
        append(&mut entries, &mut leaf, &stamp, entry, "label");
    }
    let name = session.name();
    if pi_name_of(&entries) != name {
        append(
            &mut entries,
            &mut leaf,
            &stamp,
            json!({"type": "session_info", "name": name.unwrap_or_default()}),
            "name",
        );
    }

    if let Some(configuration) = session.value(LANE_CONFIG, "main").cloned() {
        let leaf_value = leaf.clone().map(Value::from);
        let resolved = build_session_context(
            &entries,
            match &leaf_value {
                Some(id) => Leaf::Id(id),
                None => Leaf::Root,
            },
        );
        let model = &configuration["model"];
        let resolved_model = resolved.model.clone().unwrap_or(Value::Null);
        if resolved_model.get("provider") != model.get("provider")
            || resolved_model.get("modelId") != model.get("modelId")
        {
            let mut entry = json!({"type": "model_change"});
            for (key, value) in model.as_object().into_iter().flatten() {
                entry[key] = value.clone();
            }
            append(&mut entries, &mut leaf, &stamp, entry, "model");
        }
        if resolved.thinking_level.as_ref() != configuration.get("thinkingLevel") {
            let mut entry = json!({"type": "thinking_level_change"});
            if let Some(level) = configuration.get("thinkingLevel") {
                entry["thinkingLevel"] = level.clone();
            }
            append(&mut entries, &mut leaf, &stamp, entry, "thinking");
        }
    }

    let header = json!({
        "type": "session", "version": PI_SESSION_VERSION, "id": metadata.id,
        "timestamp": js::iso_string(metadata.created_at), "cwd": metadata.cwd,
    });
    let output = PathBuf::from(session_journal::repo::absolute(&output_path.display().to_string()));
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut content = String::new();
    for line in std::iter::once(&header).chain(entries.iter()) {
        content.push_str(&serde_json::to_string(line).map_err(|e| e.to_string())?);
        content.push('\n');
    }
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&output)
        .map_err(|e| export_open_error(&e, &output))?;
    file.write_all(content.as_bytes())
        .map_err(|e| format!("{}: {e}", output.display()))?;
    Ok(ExportResult {
        session_id: metadata.id.clone(),
        path: output,
        entries: entries.len(),
    })
}

/// Node's `open(..., "wx")` error text.
fn export_open_error(error: &std::io::Error, path: &Path) -> String {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return format!("EEXIST: file already exists, open '{}'", path.display());
    }
    format!("{error}: open '{}'", path.display())
}
