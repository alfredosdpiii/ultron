//! The positional system prompt (spec §7.4): `pi.system` entries that make the replayed sections and tools of the
//! context equal the desired ones. Port of upstream `harness/prompt.ts`. In Ultron the tool list is exactly the REPL
//! tool the cell executor declares.

use ai::{Message, Tool, ToolReference};
use durable::EntryDraft;
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value, json};

use super::context::ContextView;
use super::entries;

/// Sections in effect after replaying system messages in order: set in place, `null` deletes, re-adding appends.
pub fn replay_sections(messages: &[Message]) -> IndexMap<String, String> {
    let mut shown = IndexMap::new();
    for message in messages {
        let Message::System(system) = message else { continue };
        for (key, value) in system.sections.iter().flatten() {
            match value {
                None => {
                    shown.shift_remove(key);
                }
                Some(value) => {
                    shown.insert(key.clone(), value.clone());
                }
            }
        }
    }
    shown
}

struct ToolChanges {
    removed: Vec<ToolReference>,
    added: Vec<Tool>,
}

/// Plan the `pi.system` entries that make the replayed sections and tools of `view` equal `desired` and `tools` in
/// values and order: a complete baseline after a head marker with no later `pi.system` entry (omitting every
/// retained earlier one), else a remove-all/re-add-all pair when a minimal patch would leave a different order, else
/// the minimal patch, or nothing. Tool changes ride on the last planned entry, or on one entry of their own.
pub fn plan_system_entries(
    view: &ContextView,
    desired: &IndexMap<String, String>,
    tools: &[Tool],
    timestamp: u64,
) -> Vec<EntryDraft> {
    if let Some(head) = &view.head
        && !view
            .entries
            .iter()
            .any(|entry| entry.kind == entries::SYSTEM && entry.id > head.id)
    {
        let edits: Vec<Value> = view
            .entries
            .iter()
            .filter(|entry| entry.kind == entries::SYSTEM)
            .map(|entry| json!({ "target": entry.id.0, "action": "omit" }))
            .collect();
        let sections: Map<String, Value> = desired.iter().map(|(key, value)| (key.clone(), json!(value))).collect();
        let changes = ToolChanges {
            removed: Vec::new(),
            added: tools.to_vec(),
        };
        let mut baseline = system_entry(Some(sections), Some(&changes), timestamp);
        if !edits.is_empty() {
            baseline.edits = Some(edits);
        }
        return vec![baseline];
    }
    let sections = plan_sections(&replay_sections(&view.messages), desired);
    let changes = plan_tools(&ai::transcript::current_tools(&view.messages), tools);
    if changes.removed.is_empty() && changes.added.is_empty() {
        return sections
            .into_iter()
            .map(|patch| system_entry(Some(patch), None, timestamp))
            .collect();
    }
    if sections.is_empty() {
        return vec![system_entry(None, Some(&changes), timestamp)];
    }
    let last = sections.len() - 1;
    sections
        .into_iter()
        .enumerate()
        .map(|(index, patch)| system_entry(Some(patch), (index == last).then_some(&changes), timestamp))
        .collect()
}

/// Tool changes from `offered` to `desired`; a changed declaration is removed and re-added. When replay would not
/// yield the desired order, every offered tool is removed and every desired tool re-added in order.
fn plan_tools(offered: &[Tool], desired: &[Tool]) -> ToolChanges {
    let wanted: IndexMap<&str, &Tool> = desired.iter().map(|tool| (tool.name.as_str(), tool)).collect();
    let kept: Vec<&Tool> = offered
        .iter()
        .filter(|tool| wanted.get(tool.name.as_str()).is_some_and(|next| *next == *tool))
        .collect();
    let kept_names: IndexSet<&str> = kept.iter().map(|tool| tool.name.as_str()).collect();
    let added: Vec<&Tool> = desired
        .iter()
        .filter(|tool| !kept_names.contains(tool.name.as_str()))
        .collect();
    let replayed: Vec<&str> = kept.iter().chain(added.iter()).map(|tool| tool.name.as_str()).collect();
    if replayed
        .iter()
        .zip(desired)
        .any(|(name, tool)| *name != tool.name.as_str())
    {
        return ToolChanges {
            removed: offered
                .iter()
                .map(|tool| ToolReference {
                    name: tool.name.clone(),
                })
                .collect(),
            added: desired.to_vec(),
        };
    }
    ToolChanges {
        removed: offered
            .iter()
            .filter(|tool| !kept_names.contains(tool.name.as_str()))
            .map(|tool| ToolReference {
                name: tool.name.clone(),
            })
            .collect(),
        added: added.into_iter().cloned().collect(),
    }
}

/// Section patches: none, the minimal patch, or a remove-all/re-add-all pair when the order would differ.
fn plan_sections(shown: &IndexMap<String, String>, desired: &IndexMap<String, String>) -> Vec<Map<String, Value>> {
    let patched: Vec<&String> = shown
        .keys()
        .filter(|key| desired.contains_key(*key))
        .chain(desired.keys().filter(|key| !shown.contains_key(*key)))
        .collect();
    if patched.iter().zip(desired.keys()).any(|(a, b)| *a != b) {
        let removal = shown.keys().map(|key| (key.clone(), Value::Null)).collect();
        let readd = desired.iter().map(|(key, value)| (key.clone(), json!(value))).collect();
        return vec![removal, readd];
    }
    let mut patch = Map::new();
    for (key, value) in shown {
        match desired.get(key) {
            Some(next) if next == value => {}
            Some(next) => {
                patch.insert(key.clone(), json!(next));
            }
            None => {
                patch.insert(key.clone(), Value::Null);
            }
        }
    }
    for (key, value) in desired {
        if !shown.contains_key(key) {
            patch.insert(key.clone(), json!(value));
        }
    }
    if patch.is_empty() { Vec::new() } else { vec![patch] }
}

fn system_entry(sections: Option<Map<String, Value>>, tools: Option<&ToolChanges>, timestamp: u64) -> EntryDraft {
    let mut message = Map::new();
    message.insert("role".into(), json!("system"));
    message.insert("content".into(), json!(""));
    if let Some(sections) = sections {
        message.insert("sections".into(), Value::Object(sections));
    }
    if let Some(tools) = tools {
        if !tools.removed.is_empty() {
            message.insert("toolsRemoved".into(), json!(tools.removed));
        }
        if !tools.added.is_empty() {
            message.insert("toolsAdded".into(), json!(tools.added));
        }
    }
    message.insert("timestamp".into(), json!(timestamp));
    EntryDraft {
        model: Some(vec![Value::Object(message)]),
        ..EntryDraft::new(entries::SYSTEM)
    }
}
