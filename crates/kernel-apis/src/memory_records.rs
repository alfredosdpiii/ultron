//! Typed memory records (idea from Mirror's `memory_record.rs`, fitted to Ultron's journal + Hindsight memory).
//!
//! `memory.propose` may carry a `type` (preference, fact, procedure, pitfall, reference), a `situation` (one short
//! line: when a future session should use the record), a stable `key` (a kebab-case handle: proposing the same key
//! again replaces that record instead of adding another) and `pathGlobs` (repository-relative file patterns the
//! record applies to). The text still lives only in Hindsight and the session journal still holds hashes only; the
//! typed metadata goes to a [`RecordIndex`] every session of the machine shares, so a later session can find a
//! record by key, point at it when a matching file changes ([`crate::memory_pointers`]) and read it by id.
//!
//! Bounds (bytes of UTF-8): content per type ([`content_cap`]): pitfall 1 KiB, preference 2 KiB, fact and reference
//! 4 KiB, procedure 8 KiB; situation 200 bytes on one line; key 64 bytes; at most 16 globs of at most 256 bytes;
//! source 1 KiB. Records without these fields are untyped and behave exactly as before.

use crate::memory::MemoryError;
use crate::store::{DocumentStore, MemoryDocumentStore};
use crate::util::lock;
use fancy_regex::Regex;
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const RECORD_TYPES: [&str; 5] = ["preference", "fact", "procedure", "pitfall", "reference"];
pub const MAX_SITUATION_BYTES: usize = 200;
pub const MAX_KEY_BYTES: usize = 64;
pub const MAX_GLOBS: usize = 16;
pub const MAX_GLOB_BYTES: usize = 256;
pub const MAX_SOURCE_BYTES: usize = 1024;

/// The most content (UTF-8 bytes) a record of `kind` may hold.
pub fn content_cap(kind: &str) -> usize {
    match kind {
        "pitfall" => 1024,
        "preference" => 2048,
        "procedure" => 8192,
        _ => 4096,
    }
}

/// The typed part of a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    pub kind: String,
    pub key: Option<String>,
    pub situation: String,
    pub path_globs: Vec<String>,
    /// Where an imported record came from (`ultron memory import`); never set from the REPL.
    pub source: Option<String>,
}

impl Typed {
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("type".into(), self.kind.clone().into());
        if let Some(key) = &self.key {
            out.insert("key".into(), key.clone().into());
        }
        out.insert("situation".into(), self.situation.clone().into());
        out.insert("pathGlobs".into(), json!(self.path_globs));
        if let Some(source) = &self.source {
            out.insert("source".into(), source.clone().into());
        }
        Value::Object(out)
    }

    /// Every typed text the sensitive check must see besides the content.
    pub fn screened(&self) -> String {
        let mut parts = vec![self.situation.clone()];
        parts.extend(self.key.clone());
        parts.extend(self.path_globs.iter().cloned());
        parts.join("\n")
    }
}

const TYPED_FIELDS: [&str; 5] = ["type", "key", "situation", "pathGlobs", "source"];

fn invalid() -> MemoryError {
    MemoryError::code("INVALID_INPUT")
}

fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

/// A kebab-case slug: lowercase ASCII letters and digits joined by single dashes.
pub fn is_kebab(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && !key.starts_with('-')
        && !key.ends_with('-')
        && !key.contains("--")
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The typed fields of a propose payload: `None` for an untyped record; INVALID_INPUT when a field is malformed,
/// `type` or `situation` is missing, or `text` is over the type's cap.
pub fn typed_fields(payload: &Map<String, Value>, text: &str) -> Result<Option<Typed>, MemoryError> {
    if !TYPED_FIELDS.iter().any(|field| payload.contains_key(*field)) {
        return Ok(None);
    }
    let kind = payload
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| RECORD_TYPES.contains(kind))
        .ok_or_else(invalid)?
        .to_string();
    let situation = payload.get("situation").and_then(Value::as_str).ok_or_else(invalid)?;
    let situation = situation.trim();
    if situation.is_empty() || situation.len() > MAX_SITUATION_BYTES || has_control(situation) {
        return Err(invalid());
    }
    let key = match payload.get("key") {
        None => None,
        Some(Value::String(key)) if is_kebab(key) => Some(key.clone()),
        Some(_) => return Err(invalid()),
    };
    let path_globs = match payload.get("pathGlobs") {
        None => Vec::new(),
        Some(Value::Array(items)) if items.len() <= MAX_GLOBS => {
            let mut globs: Vec<String> = Vec::new();
            for item in items {
                let glob = item.as_str().ok_or_else(invalid)?;
                Glob::new(glob)?;
                if !globs.iter().any(|seen| seen == glob) {
                    globs.push(glob.to_string());
                }
            }
            globs
        }
        Some(_) => return Err(invalid()),
    };
    let source = match payload.get("source") {
        None => None,
        Some(Value::String(source)) if source.len() <= MAX_SOURCE_BYTES && !has_control(source) => Some(source.clone()),
        Some(_) => return Err(invalid()),
    };
    if text.len() > content_cap(&kind) {
        return Err(invalid());
    }
    Ok(Some(Typed {
        kind,
        key,
        situation: situation.to_string(),
        path_globs,
        source,
    }))
}

// --- globs ---------------------------------------------------------------------------------------------------------

/// A repository-relative file pattern: `*` and `?` stay within one path segment, `**` as a whole segment spans any
/// number of segments (also none), `[abc]` / `[!abc]` match one character of a set, `{a,b}` either alternative.
#[derive(Debug, Clone)]
pub struct Glob {
    regex: Regex,
}

impl Glob {
    /// INVALID_INPUT for an empty, absolute or escaping (`..`) pattern, `//`, an unbalanced `[`/`{` or one over
    /// 256 bytes.
    pub fn new(pattern: &str) -> Result<Glob, MemoryError> {
        if pattern.is_empty()
            || pattern.len() > MAX_GLOB_BYTES
            || pattern.starts_with('/')
            || pattern.contains("//")
            || pattern.contains('\\')
            || has_control(pattern)
            || pattern.split('/').any(|segment| segment == "..")
        {
            return Err(invalid());
        }
        let source = translate(pattern).ok_or_else(invalid)?;
        let regex = Regex::new(&source).map_err(|_| invalid())?;
        Ok(Glob { regex })
    }

    /// Whether the repository-relative `path` (with `/`) matches.
    pub fn matches(&self, path: &str) -> bool {
        self.regex.is_match(path).unwrap_or(false)
    }
}

/// Whether `path` matches `pattern`; an invalid pattern matches nothing.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    Glob::new(pattern).is_ok_and(|glob| glob.matches(path))
}

fn escape(ch: char, out: &mut String) {
    if "\\.+*?()|[]{}^$#&-~".contains(ch) {
        out.push('\\');
    }
    out.push(ch);
}

fn translate(pattern: &str) -> Option<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("^");
    let mut index = 0;
    let mut in_brace = false;
    while index < chars.len() {
        let ch = chars[index];
        match ch {
            '*' if chars.get(index + 1) == Some(&'*') => {
                let starts = index == 0 || chars[index - 1] == '/';
                let next = chars.get(index + 2);
                if !starts || !matches!(next, None | Some('/')) {
                    return None;
                }
                if next == Some(&'/') {
                    // `**/`: any number of whole segments, also none.
                    out.push_str("(?:[^/]+/)*");
                    index += 3;
                } else if index == 0 {
                    out.push_str(".*");
                    index += 2;
                } else {
                    // `dir/**`: everything below dir.
                    out.pop();
                    out.push_str("(?:/.*)?");
                    index += 2;
                }
                continue;
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '[' => {
                let end = chars[index + 1..].iter().position(|c| *c == ']')? + index + 1;
                let mut set: Vec<char> = chars[index + 1..end].to_vec();
                let negated = set.first() == Some(&'!');
                if negated {
                    set.remove(0);
                }
                if set.is_empty() || set.contains(&'/') || set.contains(&'[') {
                    return None;
                }
                out.push('[');
                if negated {
                    out.push('^');
                    out.push('/');
                }
                for c in set {
                    if c != '-' && "\\^&~".contains(c) {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push(']');
                index = end + 1;
                continue;
            }
            ']' => return None,
            '{' if !in_brace => {
                in_brace = true;
                out.push_str("(?:");
            }
            ',' if in_brace => out.push('|'),
            '}' if in_brace => {
                in_brace = false;
                out.push(')');
            }
            '{' | '}' => return None,
            other => escape(other, &mut out),
        }
        index += 1;
    }
    if in_brace {
        return None;
    }
    out.push('$');
    Some(out)
}

// --- the shared record index -----------------------------------------------------------------------------------------

/// One indexed record: its Hindsight document id, typed fields, scope and the identity tags it was stored with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub id: String,
    pub typed: Typed,
    pub scope: String,
    pub tags: Vec<String>,
    pub text_hash: String,
    pub updated_at: String,
}

impl IndexEntry {
    fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("id".into(), self.id.clone().into());
        if let Value::Object(typed) = self.typed.to_json() {
            out.extend(typed);
        }
        out.insert("scope".into(), self.scope.clone().into());
        out.insert("tags".into(), json!(self.tags));
        out.insert("textHash".into(), self.text_hash.clone().into());
        out.insert("updatedAt".into(), self.updated_at.clone().into());
        Value::Object(out)
    }

    fn from_json(value: &Value) -> Option<IndexEntry> {
        let map = value.as_object()?;
        let text = |key: &str| map.get(key).and_then(Value::as_str).map(str::to_string);
        let strings = |key: &str| -> Option<Vec<String>> {
            map.get(key)?
                .as_array()?
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect()
        };
        Some(IndexEntry {
            id: text("id")?,
            typed: Typed {
                kind: text("type").filter(|kind| RECORD_TYPES.contains(&kind.as_str()))?,
                key: text("key"),
                situation: text("situation")?,
                path_globs: strings("pathGlobs")?,
                source: text("source"),
            },
            scope: text("scope")?,
            tags: strings("tags")?,
            text_hash: text("textHash")?,
            updated_at: text("updatedAt").unwrap_or_default(),
        })
    }

    /// Whether a session whose scope filters are `filters` sees this record (same scope, same identity tags).
    pub fn visible(&self, filters: &Map<String, Value>) -> bool {
        let Some(tags) = filters.get(&self.scope).and_then(Value::as_array) else {
            return false;
        };
        tags.len() == self.tags.len()
            && tags
                .iter()
                .all(|tag| tag.as_str().is_some_and(|tag| self.tags.iter().any(|own| own == tag)))
    }
}

/// The typed records of every session on the machine: `{"version": 1, "records": [...]}` in one document. Writes
/// read, change and replace the document while holding an exclusive lock on `<path>.lock` (file-backed), so two
/// sessions never lose each other's records.
pub struct RecordIndex {
    store: Arc<dyn DocumentStore>,
    lock_path: Option<PathBuf>,
    local: Mutex<()>,
}

impl Default for RecordIndex {
    fn default() -> Self {
        RecordIndex::in_memory()
    }
}

impl RecordIndex {
    pub fn in_memory() -> RecordIndex {
        RecordIndex {
            store: Arc::new(MemoryDocumentStore::default()),
            lock_path: None,
            local: Mutex::new(()),
        }
    }

    /// The index file every session of an agent directory shares.
    pub fn file(path: impl Into<PathBuf>) -> RecordIndex {
        let path = path.into();
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        RecordIndex {
            store: Arc::new(crate::store::FileDocumentStore::new(path)),
            lock_path: Some(lock_path),
            local: Mutex::new(()),
        }
    }

    /// `<agent dir>/memory/records.json`.
    pub fn for_agent_dir(agent_dir: &std::path::Path) -> RecordIndex {
        RecordIndex::file(agent_dir.join("memory").join("records.json"))
    }

    fn parse(document: Option<Value>) -> Result<Vec<IndexEntry>, String> {
        let Some(document) = document else {
            return Ok(Vec::new());
        };
        if document["version"] != 1 {
            return Err("unsupported memory record index".into());
        }
        Ok(document["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(IndexEntry::from_json)
            .collect())
    }

    /// Every indexed record, in the order they were first indexed.
    pub fn entries(&self) -> Result<Vec<IndexEntry>, String> {
        Self::parse(self.store.read()?)
    }

    /// The record with document id `id`.
    pub fn entry(&self, id: &str) -> Result<Option<IndexEntry>, String> {
        Ok(self.entries()?.into_iter().find(|entry| entry.id == id))
    }

    /// The visible record holding `key` in `scope`.
    pub fn by_key(&self, key: &str, scope: &str, filters: &Map<String, Value>) -> Result<Option<IndexEntry>, String> {
        Ok(self
            .entries()?
            .into_iter()
            .find(|entry| entry.scope == scope && entry.typed.key.as_deref() == Some(key) && entry.visible(filters)))
    }

    fn change(&self, change: impl FnOnce(&mut Vec<IndexEntry>)) -> Result<(), String> {
        let _local = lock(&self.local);
        let _file = match &self.lock_path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(path)
                    .map_err(|e| e.to_string())?;
                file.lock().map_err(|e| e.to_string())?;
                Some(file)
            }
            None => None,
        };
        let mut entries = Self::parse(self.store.read()?)?;
        change(&mut entries);
        let records: Vec<Value> = entries.iter().map(IndexEntry::to_json).collect();
        self.store.write(&json!({"version": 1, "records": records}))
    }

    /// Add `entry`, replacing the record with its id (it keeps its place).
    pub fn upsert(&self, entry: IndexEntry) -> Result<(), String> {
        self.change(|entries| match entries.iter_mut().find(|e| e.id == entry.id) {
            Some(existing) => *existing = entry,
            None => entries.push(entry),
        })
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.change(|entries| entries.retain(|entry| entry.id != id))
    }
}
