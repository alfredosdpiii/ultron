//! `jsonl/legacy-v3.ts`: a Pi v3 session file (the format `crates/session-file` writes) read as format-4 writes.
//! Retained records (messages, custom, custom messages, branch summaries, compactions) become entries with new
//! UUIDv7 ids minted from their timestamps; model, thinking-level, active-tools, session-info and label records
//! become the derived current values (name, labels, `main` tip, lane configuration).

use crate::Clock;
use crate::codec::{self, JSONL_FORMAT_VERSION, JSONL_STORAGE_VERSION, ParsedHeader};
use crate::usage::{add_usage, empty_usage, num};
use serde_json::{Map, Value, json};
use session_file::js;
use std::collections::{HashMap, HashSet};
use std::path::Path;

const RETAINED: [&str; 5] = ["message", "custom", "custom_message", "branch_summary", "compaction"];
const ALL: [&str; 10] = [
    "message",
    "custom",
    "custom_message",
    "branch_summary",
    "compaction",
    "model_change",
    "thinking_level_change",
    "active_tools_change",
    "session_info",
    "label",
];

#[derive(Clone, Debug)]
struct Indexed {
    id: String,
    parent_id: Option<String>,
    /// This node's new id, or its parent's mapped id when the node is discarded.
    mapped_id: Option<String>,
    kind: String,
    /// Sequence of a retained node.
    seq: i64,
    /// The record itself (structure and metadata only are needed, but the record is small to keep).
    record: Value,
}

impl Indexed {
    fn retained(&self) -> bool {
        RETAINED.contains(&self.kind.as_str())
    }
}

/// `LegacyV3Source`: the captured complete records of a v3 file and the writes they normalize to.
#[derive(Clone, Debug)]
pub struct LegacyV3Source {
    pub header: Map<String, Value>,
    pub imported_usage: Value,
    pub next_seq: i64,
    pub values: Vec<Value>,
    entries: Vec<Indexed>,
    by_id: HashMap<String, usize>,
}

fn timestamp_ms(record: &Value) -> Option<i64> {
    record.get("timestamp").and_then(Value::as_str).and_then(js::parse_date)
}

fn ts_value(record: &Value) -> Value {
    timestamp_ms(record).map_or(Value::Null, Value::from)
}

fn set_value(namespace: &str, key: &str, value: Value, seq: i64) -> Value {
    json!({"kind": "value", "op": "set", "namespace": namespace, "key": key, "value": value, "seq": seq})
}

/// `metadataFromLegacyV3Header` as a normalized format-4 header (without `nextSeq`).
pub fn normalize_header(header: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("v".into(), JSONL_FORMAT_VERSION.into());
    out.insert("kind".into(), "header".into());
    out.insert("id".into(), header["id"].clone());
    out.insert(
        "createdAt".into(),
        codec::header_str(header, "timestamp")
            .and_then(js::parse_date)
            .map_or(Value::Null, Value::from),
    );
    out.insert("storageVersion".into(), JSONL_STORAGE_VERSION.into());
    out.insert("cwd".into(), header["cwd"].clone());
    if let Some(parent) = codec::header_str(header, "parentSession") {
        match parent_session_id(parent) {
            Some(id) => out.insert("parentSessionId".into(), id.into()),
            None => out.insert("legacyParentSessionPath".into(), parent.into()),
        };
    }
    out
}

/// The id in the header of the parent session file, when it has a readable one.
fn parent_session_id(path: &str) -> Option<String> {
    let line = crate::io::read_first_line(Path::new(path)).ok()??;
    match codec::parse_header(&line).ok()? {
        ParsedHeader::V4(h) | ParsedHeader::V3(h) => codec::header_str(&h, "id").map(str::to_string),
    }
}

fn parse_record(line: &str) -> Result<Value, String> {
    let record: Value =
        serde_json::from_str(line).map_err(|_| "Invalid legacy v3 JSONL record: not valid JSON".to_string())?;
    let kind = record.get("type");
    match kind.and_then(Value::as_str) {
        Some(t) if ALL.contains(&t) => Ok(record),
        _ => Err(format!(
            "Unsupported legacy v3 record type: {}",
            match kind {
                None => "undefined".to_string(),
                Some(Value::String(s)) => s.clone(),
                Some(other) => js::stringify(other),
            }
        )),
    }
}

fn legacy_usage(record: &Value) -> Option<&Value> {
    match record["type"].as_str()? {
        "message" => {
            let message = &record["message"];
            matches!(message["role"].as_str(), Some("assistant" | "toolResult"))
                .then(|| message.get("usage"))
                .flatten()
        }
        "compaction" | "branch_summary" => record.get("usage"),
        _ => None,
    }
}

fn imported_custom_message(record: &Value) -> Value {
    let mut message = Map::new();
    message.insert("role".into(), "custom".into());
    message.insert("customType".into(), record["customType"].clone());
    message.insert("content".into(), record["content"].clone());
    if let Some(details) = record.get("details") {
        message.insert("details".into(), details.clone());
    }
    message.insert("display".into(), record.get("display").cloned().unwrap_or(Value::Null));
    message.insert("timestamp".into(), ts_value(record));
    Value::Object(message)
}

/// `createBranchSummaryMessage`.
pub fn branch_summary_message(summary: &Value, from_id: Value, timestamp: Value) -> Value {
    json!({"role": "branchSummary", "summary": summary, "fromId": from_id, "timestamp": timestamp})
}

/// `createCompactionSummaryMessage`.
pub fn compaction_summary_message(summary: &Value, tokens_before: &Value, timestamp: Value) -> Value {
    json!({"role": "compactionSummary", "summary": summary, "tokensBefore": tokens_before, "timestamp": timestamp})
}

impl LegacyV3Source {
    /// `LegacyV3Source.read`: scan complete records (an unterminated final line is ignored).
    pub fn read(path: &Path, clock: &mut dyn Clock) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to open legacy v3 source {}: {e}", path.display()))?;
        let lines = crate::io::complete_lines(&text);
        let Some(first) = lines.first().filter(|l| !l.is_empty()) else {
            return Err(format!("Invalid JSONL storage {}: missing header", path.display()));
        };
        let header = match codec::parse_header(first) {
            Ok(ParsedHeader::V3(h)) => h,
            Ok(ParsedHeader::V4(_)) => {
                return Err(format!(
                    "Invalid legacy v3 JSONL storage {}: expected format 3 header",
                    path.display()
                ));
            }
            Err(_) => return Err(format!("Invalid JSONL storage {}: invalid header", path.display())),
        };
        let mut entries: Vec<Indexed> = Vec::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut next_seq = 1;
        let mut imported_usage = empty_usage();
        let mut name: Option<Value> = None;
        let mut final_id: Option<String> = None;
        for line in &lines[1..] {
            let line_number = entries.len() + 2;
            let record = parse_record(line)?;
            let id = record.get("id").and_then(Value::as_str).unwrap_or("null").to_string();
            if by_id.contains_key(&id) {
                return Err(format!("Duplicate legacy v3 entry id: {id}"));
            }
            let parent_id = record.get("parentId").and_then(Value::as_str).map(str::to_string);
            let mapped_parent = match &parent_id {
                None => Some(None),
                Some(parent) => by_id.get(parent).map(|&i| entries[i].mapped_id.clone()),
            };
            let Some(mapped_parent) = mapped_parent else {
                return Err(format!(
                    "Legacy v3 entry {id} has a missing or forward parent at line {line_number}: {}",
                    parent_id.as_deref().unwrap_or("null")
                ));
            };
            let kind = record["type"].as_str().unwrap_or("").to_string();
            let mut indexed = Indexed {
                id: id.clone(),
                parent_id,
                mapped_id: mapped_parent,
                kind: kind.clone(),
                seq: 0,
                record: Value::Null,
            };
            if indexed.retained() {
                indexed.mapped_id = Some(clock.uuidv7(timestamp_ms(&record).unwrap_or(0)));
                indexed.seq = next_seq;
                next_seq += 1;
            }
            if kind == "session_info" {
                name = record.get("name").cloned();
            }
            if let Some(usage) = legacy_usage(&record) {
                imported_usage = add_usage(&imported_usage, usage);
            }
            indexed.record = record;
            by_id.insert(id.clone(), entries.len());
            entries.push(indexed);
            final_id = Some(id);
        }
        let mut source = LegacyV3Source {
            header: normalize_header(&header),
            imported_usage,
            next_seq,
            values: Vec::new(),
            entries,
            by_id,
        };
        source.values = source.normalize_values(name, final_id.as_deref(), next_seq)?;
        source.next_seq = next_seq + source.values.len() as i64;
        Ok(source)
    }

    fn resolve(&self, legacy: Option<&str>) -> Result<Option<String>, String> {
        match legacy {
            None => Ok(None),
            Some(id) => self
                .by_id
                .get(id)
                .map(|&i| self.entries[i].mapped_id.clone())
                .ok_or_else(|| format!("Missing legacy v3 entry reference: {id}")),
        }
    }

    fn resolve_value(&self, legacy: Option<&str>) -> Result<Value, String> {
        Ok(self.resolve(legacy)?.map_or(Value::Null, Value::from))
    }

    /// `resolveBranchSummaryFromId`: the legacy "root" sentinel is null.
    fn resolve_from_id(&self, from: Option<&str>) -> Result<Value, String> {
        if from == Some("root") {
            return Ok(Value::Null);
        }
        self.resolve_value(from)
    }

    fn normalize_values(
        &self,
        name: Option<Value>,
        final_id: Option<&str>,
        mut seq: i64,
    ) -> Result<Vec<Value>, String> {
        let mut values = Vec::new();
        if let Some(name) = name.filter(|n| js::truthy(Some(n))) {
            values.push(set_value("pi.session.name", "", name, seq));
            seq += 1;
        }
        let mut labels: Vec<(String, Value)> = Vec::new();
        for entry in self.entries.iter().filter(|e| e.kind == "label") {
            let Some(target) = self.resolve(entry.record.get("targetId").and_then(Value::as_str))? else {
                continue;
            };
            let label = entry.record.get("label").cloned().unwrap_or(Value::Null);
            let existing = labels.iter().position(|(t, _)| *t == target);
            match (existing, js::truthy(Some(&label))) {
                (Some(i), true) => labels[i].1 = label,
                (None, true) => labels.push((target, label)),
                (Some(i), false) => {
                    labels.remove(i);
                }
                (None, false) => {}
            }
        }
        for (target, label) in labels {
            values.push(set_value("pi.entry.label", &target, label, seq));
            seq += 1;
        }
        values.push(set_value("pi.branch.tip", "main", self.resolve_value(final_id)?, seq));
        seq += 1;
        if let Some(configuration) = self.selected_configuration(final_id) {
            values.push(set_value("pi.lane.config", "main", configuration, seq));
            values.push(set_value(
                "pi.lane.state",
                "main",
                json!({"currentOperationId": null, "lastOperationId": null, "inbox": []}),
                seq + 1,
            ));
        }
        Ok(values)
    }

    fn selected_configuration(&self, selected: Option<&str>) -> Option<Value> {
        let mut remaining: HashSet<&str> = ["model_change", "thinking_level_change", "active_tools_change"].into();
        let (mut model, mut thinking, mut tools) = (None, None, None);
        let mut current = selected.map(str::to_string);
        while let Some(id) = current {
            if remaining.is_empty() {
                break;
            }
            let entry = &self.entries[self.by_id[&id]];
            if remaining.remove(entry.kind.as_str()) {
                let r = &entry.record;
                match entry.kind.as_str() {
                    "model_change" => model = Some(json!({"provider": r["provider"], "modelId": r["modelId"]})),
                    "thinking_level_change" => thinking = Some(r.get("thinkingLevel").cloned().unwrap_or(Value::Null)),
                    _ => tools = Some(r.get("activeToolNames").cloned().unwrap_or(json!([]))),
                }
            }
            current = entry.parent_id.clone();
        }
        let (model, thinking) = (model?, thinking?);
        Some(json!({"model": model, "thinkingLevel": thinking, "activeToolNames": tools.unwrap_or(json!([]))}))
    }

    /// Retained entries' `(id, parentId, seq)` with new ids.
    pub fn entry_structures(&self) -> Vec<(String, Option<String>, i64)> {
        self.entries
            .iter()
            .filter(|e| e.retained())
            .map(|e| {
                let parent = self.resolve(e.parent_id.as_deref()).ok().flatten();
                (e.mapped_id.clone().unwrap_or_default(), parent, e.seq)
            })
            .collect()
    }

    /// `translateForkEntryId`.
    pub fn translate_fork_entry_id(&self, legacy: &str) -> Result<String, String> {
        let entry = self
            .by_id
            .get(legacy)
            .map(|&i| &self.entries[i])
            .ok_or_else(|| format!("Legacy v3 fork entry does not exist: {legacy}"))?;
        if !entry.retained() {
            return Err(format!("Legacy v3 fork entry is not a retained entry: {legacy}"));
        }
        Ok(entry.mapped_id.clone().unwrap_or_default())
    }

    /// `retainedTailStructure`: from a compaction's parent up to its `firstKeptEntryId`, inclusive.
    fn retained_tail(&self, compaction: &Indexed) -> Result<Vec<usize>, String> {
        let first_kept = compaction.record.get("firstKeptEntryId").and_then(Value::as_str);
        let mut out = Vec::new();
        let mut current = compaction.parent_id.clone();
        while let Some(id) = current {
            let index = self.by_id[&id];
            out.push(index);
            if Some(id.as_str()) == first_kept {
                return Ok(out);
            }
            current = self.entries[index].parent_id.clone();
        }
        Err(format!(
            "Legacy v3 compaction {} firstKeptEntryId is not on its parent branch: {}",
            compaction.id,
            first_kept.unwrap_or("undefined")
        ))
    }

    fn context_message(&self, entry: &Indexed) -> Result<Option<Value>, String> {
        let r = &entry.record;
        Ok(match entry.kind.as_str() {
            "message" => Some(r["message"].clone()),
            "custom_message" => Some(imported_custom_message(r)),
            "branch_summary" if js::truthy(r.get("summary")) => Some(branch_summary_message(
                &r["summary"],
                self.resolve_from_id(r.get("fromId").and_then(Value::as_str))?,
                ts_value(r),
            )),
            "compaction" => Some(compaction_summary_message(
                &r["summary"],
                &r["tokensBefore"],
                ts_value(r),
            )),
            _ => None,
        })
    }

    fn normalize_entry(&self, entry: &Indexed, tail: Vec<Value>) -> Result<Value, String> {
        let r = &entry.record;
        let mut out = Map::new();
        out.insert("kind".into(), "entry".into());
        out.insert("id".into(), entry.mapped_id.clone().unwrap_or_default().into());
        out.insert("parentId".into(), self.resolve_value(entry.parent_id.as_deref())?);
        out.insert("seq".into(), entry.seq.into());
        out.insert("timestamp".into(), ts_value(r));
        let copy = |out: &mut Map<String, Value>, key: &str| {
            if let Some(v) = r.get(key) {
                out.insert(key.into(), v.clone());
            }
        };
        match entry.kind.as_str() {
            "message" => {
                out.insert("type".into(), "message".into());
                out.insert("message".into(), r["message"].clone());
            }
            "custom_message" => {
                out.insert("type".into(), "message".into());
                out.insert("message".into(), imported_custom_message(r));
            }
            "branch_summary" => {
                out.insert("type".into(), "branch_summary".into());
                out.insert(
                    "fromId".into(),
                    self.resolve_from_id(r.get("fromId").and_then(Value::as_str))?,
                );
                copy(&mut out, "summary");
                copy(&mut out, "details");
                copy(&mut out, "usage");
                out.insert("fromHook".into(), r.get("fromHook").cloned().unwrap_or(false.into()));
            }
            "compaction" => {
                out.insert("type".into(), "compaction".into());
                copy(&mut out, "summary");
                out.insert("retainedTail".into(), Value::Array(tail));
                copy(&mut out, "tokensBefore");
                copy(&mut out, "details");
                copy(&mut out, "usage");
                out.insert("fromHook".into(), r.get("fromHook").cloned().unwrap_or(false.into()));
            }
            _ => {
                out.insert("type".into(), "custom".into());
                copy(&mut out, "customType");
                copy(&mut out, "data");
            }
        }
        Ok(Value::Object(out))
    }

    /// `writes(context, isEntrySelected)`: the normalized entries (optionally only selected new ids), then the
    /// derived current values.
    pub fn writes(&self, selected: Option<&dyn Fn(&str) -> bool>) -> Result<Vec<Value>, String> {
        let is_selected = |id: &str| selected.is_none_or(|f| f(id));
        let mut required = HashSet::new();
        for entry in self.entries.iter().filter(|e| e.kind == "compaction") {
            if !is_selected(entry.mapped_id.as_deref().unwrap_or("")) {
                continue;
            }
            for index in self.retained_tail(entry)? {
                let tail = &self.entries[index];
                if tail.retained() && tail.kind != "custom" {
                    required.insert(index);
                }
            }
        }
        let mut tail_messages: HashMap<usize, Value> = HashMap::new();
        let mut out = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if required.contains(&index)
                && let Some(message) = self.context_message(entry)?
            {
                tail_messages.insert(index, message);
            }
            if !entry.retained() || !is_selected(entry.mapped_id.as_deref().unwrap_or("")) {
                continue;
            }
            let mut tail = Vec::new();
            if entry.kind == "compaction" {
                for ancestor in self.retained_tail(entry)? {
                    if let Some(message) = tail_messages.get(&ancestor) {
                        tail.push(message.clone());
                    }
                }
                tail.reverse();
            }
            out.push(self.normalize_entry(entry, tail)?);
        }
        out.extend(self.values.iter().cloned());
        Ok(out)
    }

    /// The header with the source's sequence high-water mark.
    pub fn header_with_next_seq(&self) -> Map<String, Value> {
        let mut header = self.header.clone();
        header.insert("nextSeq".into(), num(self.next_seq as f64));
        header
    }
}
