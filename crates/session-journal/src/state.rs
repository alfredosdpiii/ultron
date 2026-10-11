//! `in-memory-storage-state.ts` and `commit.ts`: the complete materialized state of one session (entries, scalar
//! values, lists, usage rows, stats), commit preparation and validation. Writes and entries are JSON objects in the
//! key order TypeScript writes them, so a replayed line serializes back to the same bytes.

use crate::usage::{add_usage, empty_usage, num};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

/// A write before storage assigns its sequence (and an entry's timestamp).
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    /// `insertEntry(entry)`: the entry's fields in order (`id`, `parentId`, `type`, ...), without `seq`/`timestamp`.
    Entry(Map<String, Value>),
    /// `insertUsage(row)`: `id`, `usage`, `entryId?`, `adjustment`, `details?`.
    Usage(Map<String, Value>),
    ValueSet {
        namespace: String,
        key: String,
        value: Value,
    },
    ValueDelete {
        namespace: String,
        key: String,
    },
    ListAppend {
        namespace: String,
        key: String,
        value: Value,
    },
    ListDelete {
        namespace: String,
        key: String,
    },
}

impl Write {
    pub fn set(namespace: &str, key: &str, value: Value) -> Self {
        Write::ValueSet {
            namespace: namespace.into(),
            key: key.into(),
            value,
        }
    }
    pub fn delete(namespace: &str, key: &str) -> Self {
        Write::ValueDelete {
            namespace: namespace.into(),
            key: key.into(),
        }
    }
    pub fn append(namespace: &str, key: &str, value: Value) -> Self {
        Write::ListAppend {
            namespace: namespace.into(),
            key: key.into(),
            value,
        }
    }
    pub fn delete_list(namespace: &str, key: &str) -> Self {
        Write::ListDelete {
            namespace: namespace.into(),
            key: key.into(),
        }
    }
}

/// `commitWrite`.
pub fn commit_write(write: &Write, seq: i64, timestamp: i64) -> Value {
    let mut out = Map::new();
    match write {
        Write::Entry(entry) => {
            out.insert("kind".into(), "entry".into());
            out.extend(entry.clone());
            out.insert("seq".into(), seq.into());
            out.insert("timestamp".into(), timestamp.into());
        }
        Write::Usage(row) => {
            out.insert("kind".into(), "usage".into());
            out.extend(row.clone());
            out.insert("seq".into(), seq.into());
        }
        Write::ValueSet { namespace, key, value } | Write::ListAppend { namespace, key, value } => {
            let list = matches!(write, Write::ListAppend { .. });
            out.insert("kind".into(), (if list { "list" } else { "value" }).into());
            out.insert("op".into(), (if list { "append" } else { "set" }).into());
            out.insert("seq".into(), seq.into());
            out.insert("namespace".into(), namespace.clone().into());
            out.insert("key".into(), key.clone().into());
            out.insert("value".into(), value.clone());
        }
        Write::ValueDelete { namespace, key } | Write::ListDelete { namespace, key } => {
            let list = matches!(write, Write::ListDelete { .. });
            out.insert("kind".into(), (if list { "list" } else { "value" }).into());
            out.insert("op".into(), "delete".into());
            out.insert("seq".into(), seq.into());
            out.insert("namespace".into(), namespace.clone().into());
            out.insert("key".into(), key.clone().into());
        }
    }
    Value::Object(out)
}

/// What a commit assigned.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitResult {
    pub first_seq: i64,
    pub seqs: Vec<i64>,
    pub timestamp: i64,
    pub stats: Value,
}

pub(crate) fn seq_of(write: &Value) -> i64 {
    write
        .get("seq")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

pub(crate) fn str_of<'a>(write: &'a Value, key: &str) -> &'a str {
    write.get(key).and_then(Value::as_str).unwrap_or("")
}

fn physical_key(namespace: &str, key: &str) -> String {
    format!("{namespace}\u{0}{key}")
}

/// One stored scalar value.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredValue {
    pub namespace: String,
    pub key: String,
    pub value: Value,
    pub seq: i64,
}

/// One list element.
#[derive(Clone, Debug, PartialEq)]
pub struct ListElement {
    pub seq: i64,
    pub value: Value,
}

/// `StorageBranchScan`.
#[derive(Clone, Debug, Default)]
pub struct BranchScan {
    pub start: String,
    pub stop_at_type: Option<String>,
    pub stop_at_id: Option<String>,
    pub entry_type: Option<String>,
    pub custom_type: Option<String>,
    pub oldest_first: bool,
    pub limit: Option<usize>,
    pub cursor: Option<i64>,
}

/// `InMemoryStorageState`.
#[derive(Clone, Debug)]
pub struct State {
    entries: HashMap<String, Value>,
    entries_by_seq: Vec<Value>,
    scalars: BTreeMap<String, StoredValue>,
    lists: HashMap<String, Vec<ListElement>>,
    usage: HashMap<String, Value>,
    message_count: i64,
    usage_total: Value,
    next_seq: i64,
}

impl Default for State {
    fn default() -> Self {
        State {
            entries: HashMap::new(),
            entries_by_seq: Vec::new(),
            scalars: BTreeMap::new(),
            lists: HashMap::new(),
            usage: HashMap::new(),
            message_count: 0,
            usage_total: empty_usage(),
            next_seq: 1,
        }
    }
}

impl State {
    /// `prepareCommit`: sequences and the timestamp assigned, then validated against this state.
    pub fn prepare(&self, writes: &[Write], timestamp: i64) -> Result<(Vec<Value>, i64), String> {
        let first = self.next_seq;
        let committed: Vec<Value> = writes
            .iter()
            .enumerate()
            .map(|(i, w)| commit_write(w, first + i as i64, timestamp))
            .collect();
        self.validate(&committed)?;
        Ok((committed, first))
    }

    /// `validateCommittedWrites`.
    pub fn validate(&self, writes: &[Value]) -> Result<(), String> {
        let mut previous = self.next_seq - 1;
        let mut ids = HashSet::new();
        let mut entry_ids = HashSet::new();
        for write in writes {
            let seq = seq_of(write);
            if seq <= previous {
                return Err(format!("Non-monotonic storage sequence: {seq}"));
            }
            previous = seq;
            let kind = str_of(write, "kind");
            if kind != "entry" && kind != "usage" {
                continue;
            }
            let id = str_of(write, "id").to_string();
            if self.entries.contains_key(&id) || self.usage.contains_key(&id) || ids.contains(&id) {
                return Err(format!("Duplicate entry or usage id: {id}"));
            }
            if kind == "entry"
                && let Some(parent) = write.get("parentId").and_then(Value::as_str)
                && !self.entries.contains_key(parent)
                && !entry_ids.contains(parent)
            {
                return Err(format!("Missing parent entry: {parent}"));
            }
            if kind == "entry" {
                entry_ids.insert(id.clone());
            }
            ids.insert(id);
        }
        Ok(())
    }

    /// `applyValidated`: returns the stats after the writes.
    pub fn apply(&mut self, writes: &[Value]) -> Value {
        for write in writes {
            let namespace = str_of(write, "namespace");
            let key = str_of(write, "key");
            match (str_of(write, "kind"), str_of(write, "op")) {
                ("entry", _) => {
                    let mut entry = write.as_object().cloned().unwrap_or_default();
                    entry.shift_remove("kind");
                    let entry = Value::Object(entry);
                    if str_of(&entry, "type") == "message" {
                        self.message_count += 1;
                    }
                    self.entries.insert(str_of(&entry, "id").into(), entry.clone());
                    self.entries_by_seq.push(entry);
                }
                ("usage", _) => {
                    let mut row = write.as_object().cloned().unwrap_or_default();
                    row.shift_remove("kind");
                    let row = Value::Object(row);
                    self.usage_total = add_usage(&self.usage_total, &row["usage"]);
                    self.usage.insert(str_of(&row, "id").into(), row);
                }
                ("value", "delete") => {
                    self.scalars.remove(&physical_key(namespace, key));
                }
                ("list", "delete") => {
                    self.lists.remove(&physical_key(namespace, key));
                }
                ("value", _) => self.set_scalar(namespace, key, write["value"].clone(), seq_of(write)),
                ("list", _) => self.push_list(namespace, key, write["value"].clone(), seq_of(write)),
                _ => {}
            }
            self.next_seq = seq_of(write) + 1;
        }
        self.stats()
    }

    fn set_scalar(&mut self, namespace: &str, key: &str, value: Value, seq: i64) {
        self.scalars.insert(
            physical_key(namespace, key),
            StoredValue {
                namespace: namespace.into(),
                key: key.into(),
                value,
                seq,
            },
        );
    }

    fn push_list(&mut self, namespace: &str, key: &str, value: Value, seq: i64) {
        self.lists
            .entry(physical_key(namespace, key))
            .or_default()
            .push(ListElement { seq, value });
    }

    /// `advanceNextSeq`.
    pub fn advance_next_seq(&mut self, next: i64) -> Result<(), String> {
        if next < 1 {
            return Err(format!("Invalid storage sequence high-water mark: {next}"));
        }
        self.next_seq = self.next_seq.max(next);
        Ok(())
    }

    pub fn next_seq(&self) -> i64 {
        self.next_seq
    }

    /// `{ messageCount, usage }`.
    pub fn stats(&self) -> Value {
        json!({"messageCount": num(self.message_count as f64), "usage": self.usage_total})
    }

    pub fn entry(&self, id: &str) -> Option<&Value> {
        self.entries.get(id)
    }

    /// Entries in sequence order.
    pub fn entries(&self) -> &[Value] {
        &self.entries_by_seq
    }

    pub fn value(&self, namespace: &str, key: &str) -> Option<&StoredValue> {
        self.scalars.get(&physical_key(namespace, key))
    }

    /// `scanValues`: values of `namespace` whose key starts with `prefix`, by key code points.
    pub fn scan_values(&self, namespace: &str, prefix: &str) -> Vec<&StoredValue> {
        let mut found: Vec<&StoredValue> = self
            .scalars
            .values()
            .filter(|v| v.namespace == namespace && v.key.starts_with(prefix))
            .collect();
        found.sort_by(|l, r| l.key.cmp(&r.key));
        found
    }

    /// `readList` in ascending order (no cursor), at most `limit` (default 1000, capped at 10000).
    pub fn read_list(&self, namespace: &str, key: &str) -> Vec<&ListElement> {
        self.lists
            .get(&physical_key(namespace, key))
            .map(|items| items.iter().take(1000).collect())
            .unwrap_or_default()
    }

    /// Every list as `(namespace, key, elements)`, by physical key.
    pub fn lists(&self) -> Vec<(String, String, &[ListElement])> {
        let mut keys: Vec<&String> = self.lists.keys().collect();
        keys.sort();
        keys.into_iter()
            .map(|k| {
                let (namespace, key) = k.split_once('\u{0}').unwrap_or((k, ""));
                (namespace.to_string(), key.to_string(), self.lists[k].as_slice())
            })
            .collect()
    }

    /// Every scalar value, by physical key (namespace, NUL, key).
    pub fn values(&self) -> impl Iterator<Item = &StoredValue> {
        self.scalars.values()
    }

    /// Usage rows in sequence order.
    pub fn usage_rows(&self) -> Vec<&Value> {
        let mut rows: Vec<&Value> = self.usage.values().collect();
        rows.sort_by_key(|row| seq_of(row));
        rows
    }

    /// `scanBranch`.
    pub fn scan_branch(&self, query: &BranchScan) -> Result<Vec<Value>, String> {
        let mut entry = self
            .entries
            .get(&query.start)
            .ok_or_else(|| format!("Unknown branch start: {}", query.start))?;
        let mut path = Vec::new();
        loop {
            path.push(entry.clone());
            let Some(parent) = entry.get("parentId").and_then(Value::as_str) else {
                break;
            };
            entry = self
                .entries
                .get(parent)
                .ok_or_else(|| "Corrupt branch: missing parent".to_string())?;
        }
        if query.oldest_first {
            path.reverse();
        }
        let mut stopped = Vec::new();
        for candidate in path {
            let stop = Some(str_of(&candidate, "id")) == query.stop_at_id.as_deref()
                || Some(str_of(&candidate, "type")) == query.stop_at_type.as_deref();
            stopped.push(candidate);
            if stop {
                break;
            }
        }
        let filtered: Vec<Value> = stopped
            .into_iter()
            .filter(|c| query.entry_type.as_deref().is_none_or(|t| str_of(c, "type") == t))
            .filter(|c| {
                query
                    .custom_type
                    .as_deref()
                    .is_none_or(|t| c.get("customType").and_then(Value::as_str) == Some(t))
            })
            .filter(|c| {
                query.cursor.is_none_or(|cursor| {
                    if query.oldest_first {
                        seq_of(c) > cursor
                    } else {
                        seq_of(c) < cursor
                    }
                })
            })
            .collect();
        Ok(match query.limit {
            Some(limit) => filtered.into_iter().take(limit).collect(),
            None => filtered,
        })
    }

    /// The path from the root to `tip`, oldest first; empty for a `null` tip.
    pub fn branch_path(&self, tip: Option<&str>) -> Result<Vec<Value>, String> {
        match tip {
            None => Ok(Vec::new()),
            Some(start) => self.scan_branch(&BranchScan {
                start: start.into(),
                oldest_first: true,
                ..Default::default()
            }),
        }
    }
}
