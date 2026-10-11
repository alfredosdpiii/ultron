//! `session/context.ts`: the model context of a branch path. The latest compaction replaces what came before it;
//! context edits (`custom` entries of type `context_edit`) omit or replace earlier entries; failed, aborted and
//! deferred assistant messages are left out.

use crate::legacy_v3::{branch_summary_message, compaction_summary_message};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

pub const CONTEXT_EDIT_CUSTOM_TYPE: &str = "context_edit";
pub const CONTEXT_OMITTED_CUSTOM_TYPE: &str = "context_edit:omitted";
pub const CONTEXT_OMITTED_TOOL_RESULT_TEXT: &str = "[Tool result removed from context.]";

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `buildContextEntries`.
pub fn build_context_entries(path: &[Value]) -> Vec<Value> {
    match path.iter().rposition(|e| str_of(e, "type") == "compaction") {
        None => path.to_vec(),
        Some(i) => path[i..].to_vec(),
    }
}

fn is_context_message(message: &Value) -> bool {
    str_of(message, "role") != "assistant" || !matches!(str_of(message, "stopReason"), "error" | "aborted" | "deferred")
}

/// `sessionEntryToContextMessages`.
pub fn entry_context_messages(entry: &Value) -> Vec<Value> {
    match str_of(entry, "type") {
        "message" => {
            let message = &entry["message"];
            if is_context_message(message) {
                vec![message.clone()]
            } else {
                vec![]
            }
        }
        "compaction" => {
            let mut out = vec![compaction_summary_message(
                &entry["summary"],
                &entry["tokensBefore"],
                entry["timestamp"].clone(),
            )];
            if let Some(tail) = entry["retainedTail"].as_array() {
                out.extend(tail.iter().filter(|m| is_context_message(m)).cloned());
            }
            out
        }
        "branch_summary" if session_file::js::truthy(entry.get("summary")) => {
            vec![branch_summary_message(
                &entry["summary"],
                entry["fromId"].clone(),
                entry["timestamp"].clone(),
            )]
        }
        _ => vec![],
    }
}

fn parse_edit(value: &Value) -> Option<(String, Option<Value>)> {
    let target = value.get("targetId")?.as_str().filter(|t| !t.is_empty())?;
    match value.get("replacement")? {
        Value::Null => Some((target.into(), None)),
        Value::Object(r) => {
            let content = r.get("content")?;
            (content.is_string() || content.is_array()).then(|| (target.into(), Some(content.clone())))
        }
        _ => None,
    }
}

/// `contextEditsOf`.
fn edits_of(entry: &Value) -> Vec<(String, Option<Value>)> {
    if str_of(entry, "type") != "custom" || str_of(entry, "customType") != CONTEXT_EDIT_CUSTOM_TYPE {
        return vec![];
    }
    let Some(data) = entry.get("data").filter(|d| d.is_object()) else {
        return vec![];
    };
    match data.get("edits").and_then(Value::as_array) {
        Some(list) => list.iter().filter_map(parse_edit).collect(),
        None => parse_edit(data).into_iter().collect(),
    }
}

fn tool_call_ids(content: &Value) -> Vec<String> {
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| str_of(p, "type") == "toolCall")
                .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn replace_content(message: &Value, content: &Value) -> Value {
    let role = str_of(message, "role");
    if !matches!(role, "user" | "assistant" | "toolResult" | "custom") {
        return message.clone();
    }
    let next = match (role, content) {
        ("assistant" | "toolResult", Value::String(text)) => json!([{"type": "text", "text": text}]),
        _ => content.clone(),
    };
    let mut out = message.clone();
    out["content"] = next;
    out
}

fn omitted(entry: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "parentId", "seq", "timestamp"] {
        out.insert(key.into(), entry[key].clone());
    }
    out.insert("type".into(), "custom".into());
    out.insert("customType".into(), CONTEXT_OMITTED_CUSTOM_TYPE.into());
    Value::Object(out)
}

/// `applyContextEdits`.
pub fn apply_context_edits(entries: &[Value]) -> Vec<Value> {
    let mut edits: HashMap<String, Option<Value>> = HashMap::new();
    for entry in entries {
        for (target, replacement) in edits_of(entry) {
            edits.insert(target, replacement);
        }
    }
    if edits.is_empty() {
        return entries.to_vec();
    }
    let mut hidden = HashSet::new();
    for entry in entries {
        if str_of(entry, "type") != "message" || str_of(&entry["message"], "role") != "assistant" {
            continue;
        }
        let Some(replacement) = edits.get(str_of(entry, "id")) else {
            continue;
        };
        let kept: HashSet<String> = replacement
            .as_ref()
            .map(tool_call_ids)
            .unwrap_or_default()
            .into_iter()
            .collect();
        for id in tool_call_ids(&entry["message"]["content"]) {
            if !kept.contains(&id) {
                hidden.insert(id);
            }
        }
    }
    entries
        .iter()
        .map(|entry| {
            let id = str_of(entry, "id");
            match str_of(entry, "type") {
                "custom" => {
                    if matches!(edits.get(id), Some(None)) {
                        omitted(entry)
                    } else {
                        entry.clone()
                    }
                }
                "message" => {
                    let message = &entry["message"];
                    if str_of(message, "role") == "toolResult" && hidden.contains(str_of(message, "toolCallId")) {
                        return omitted(entry);
                    }
                    let Some(replacement) = edits.get(id) else {
                        return entry.clone();
                    };
                    let mut out = entry.clone();
                    match replacement {
                        None if str_of(message, "role") == "toolResult" => {
                            out["message"] = replace_content(message, &CONTEXT_OMITTED_TOOL_RESULT_TEXT.into());
                        }
                        None => return omitted(entry),
                        Some(content) => out["message"] = replace_content(message, content),
                    }
                    out
                }
                _ => entry.clone(),
            }
        })
        .collect()
}

/// `buildSessionContext` without entry projectors (custom entries contribute nothing).
pub fn build_session_context(path: &[Value]) -> Vec<Value> {
    apply_context_edits(&build_context_entries(path))
        .iter()
        .flat_map(entry_context_messages)
        .collect()
}

pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

fn text_user(text: String, timestamp: &Value) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}], "timestamp": timestamp})
}

/// `bashExecutionToText`.
fn bash_execution_text(m: &Value) -> String {
    let mut text = format!("Ran `{}`\n", str_of(m, "command"));
    let output = str_of(m, "output");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{output}\n```"));
    }
    if m["cancelled"] == true {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = m["exitCode"].as_i64().filter(|c| *c != 0) {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    if m["truncated"] == true && !str_of(m, "fullOutputPath").is_empty() {
        text.push_str(&format!(
            "\n\n[Output truncated. Full output: {}]",
            str_of(m, "fullOutputPath")
        ));
    }
    text
}

/// `convertToLlm`: agent messages as provider messages (custom roles become user messages).
pub fn convert_to_llm(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|m| match str_of(m, "role") {
            "bashExecution" if m["excludeFromContext"] == true => None,
            "bashExecution" => Some(text_user(bash_execution_text(m), &m["timestamp"])),
            "custom" => {
                let content = match &m["content"] {
                    Value::String(text) => json!([{"type": "text", "text": text}]),
                    other => other.clone(),
                };
                Some(json!({"role": "user", "content": content, "timestamp": m["timestamp"]}))
            }
            "branchSummary" => Some(text_user(
                format!("{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}", str_of(m, "summary")),
                &m["timestamp"],
            )),
            "compactionSummary" => Some(text_user(
                format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    str_of(m, "summary")
                ),
                &m["timestamp"],
            )),
            "system" | "user" | "assistant" | "toolResult" => Some(m.clone()),
            _ => None,
        })
        .collect()
}
