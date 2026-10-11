//! Context derivation (spec §2.1): the active transcript of a conversation and the model context derived from it.
//! Port of upstream `harness/context.ts`, reading inside one transaction instead of off the Session line.

use std::collections::HashMap;

use ai::{AssistantContent, Message, StopReason, TextContent, ToolResultMessage, UserContent};
use durable::{ConversationId, Cursor, EntryId, EntryQuery, EntryRecord, ScanOrder, Tx};
use serde_json::{Value, json};

use crate::scheduler::{HarnessError, Result};

const SCAN_PAGE_SIZE: usize = 256;
const MISSING_RESULT_TEXT: &str = "Tool result unavailable: history ends before this call completed.";

/// One committed context: the head marker, the raw active entries, each entry's contributed messages (after edits and
/// excluded stop reasons, before tool result ordering), and the model messages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextView {
    pub head: Option<EntryRecord>,
    pub entries: Vec<EntryRecord>,
    pub contributions: Vec<Vec<Message>>,
    pub messages: Vec<Message>,
}

/// The model messages an entry stores.
pub fn entry_messages(entry: &EntryRecord) -> Result<Vec<Message>> {
    entry
        .model
        .iter()
        .flatten()
        .map(|message| message_of(message.clone()))
        .collect()
}

pub(crate) fn message_of(value: Value) -> Result<Message> {
    serde_json::from_value(value).map_err(|error| HarnessError::Error(format!("Malformed model message: {error}")))
}

/// The committed context of one conversation, or of the context cut off at the visible entry `at`. Runs inside a
/// transaction; its reads ignore the read-after-write rule, so a commit may derive context after writing.
pub fn read_context(tx: &mut Tx<'_>, conversation_id: ConversationId, at: Option<EntryId>) -> Result<ContextView> {
    let tail = match at {
        None => {
            let query = EntryQuery {
                conversation_id,
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            };
            match tx.committed_scan_entries(&query, 1, None)?.items.first() {
                Some(entry) => entry.id,
                None => return Ok(ContextView::default()),
            }
        }
        Some(at) => {
            if tx.committed_visible_entry(conversation_id, at)?.is_none() {
                return Err(HarnessError::Error(format!(
                    "Entry {} is not visible from conversation {}",
                    at.0, conversation_id.0
                )));
            }
            at
        }
    };
    let head = tx.committed_head_marker(conversation_id, Some(tail))?;
    let mut entries = Vec::new();
    let query = EntryQuery {
        conversation_id,
        min_entry_id: head.as_ref().and_then(|head| head.head),
        max_entry_id: Some(tail),
        order: Some(ScanOrder::Ascending),
    };
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = tx.committed_scan_entries(&query, SCAN_PAGE_SIZE, cursor.as_ref())?;
        entries.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    derive(head, entries)
}

/// Derive the view from the entries scanned from the head marker's head, or transcript start, through the tail.
pub(crate) fn derive(head: Option<EntryRecord>, range: Vec<EntryRecord>) -> Result<ContextView> {
    // Edits of every entry in the range count, including older head markers that the active selection drops.
    let mut edits: HashMap<u64, Value> = HashMap::new();
    for entry in &range {
        for edit in entry.edits.iter().flatten() {
            if let Some(target) = edit["target"].as_u64() {
                edits.insert(target, edit.clone());
            }
        }
    }
    let active: Vec<EntryRecord> = match &head {
        None => range,
        Some(head) => std::iter::once(head.clone())
            .chain(range.into_iter().filter(|entry| entry.head.is_none()))
            .collect(),
    };
    let mut contributions = Vec::with_capacity(active.len());
    for entry in &active {
        contributions.push(contribute(entry, edits.get(&entry.id.0))?);
    }
    let messages = lead_with_system(order_tool_results(&contributions.concat()));
    Ok(ContextView {
        head,
        entries: active,
        contributions,
        messages,
    })
}

fn excluded(message: &Message) -> bool {
    matches!(message, Message::Assistant(assistant)
        if matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error | StopReason::Deferred))
}

/// One active entry's model messages after its edit and excluded stop reasons, before tool result ordering.
fn contribute(entry: &EntryRecord, edit: Option<&Value>) -> Result<Vec<Message>> {
    let messages = match edit.and_then(|edit| edit["action"].as_str()) {
        Some("omit") => return Ok(Vec::new()),
        Some("replace") => edit
            .and_then(|edit| edit["messages"].as_array())
            .into_iter()
            .flatten()
            .map(|message| message_of(message.clone()))
            .collect::<Result<Vec<_>>>()?,
        _ => entry_messages(entry)?,
    };
    Ok(messages.into_iter().filter(|message| !excluded(message)).collect())
}

/// Move a system message that only user messages precede to the front.
fn lead_with_system(mut messages: Vec<Message>) -> Vec<Message> {
    let Some(index) = messages.iter().position(|message| !matches!(message, Message::User(_))) else {
        return messages;
    };
    if index == 0 || !matches!(messages[index], Message::System(_)) {
        return messages;
    }
    let system = messages.remove(index);
    messages.insert(0, system);
    messages
}

/// Place each assistant's tool results directly after it in call order. Results are taken from the messages before
/// the next assistant; a missing result is synthesized and unmatched results are dropped.
pub fn order_tool_results(messages: &[Message]) -> Vec<Message> {
    let mut ordered = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if matches!(message, Message::ToolResult(_)) {
            continue;
        }
        ordered.push(message.clone());
        let Message::Assistant(assistant) = message else {
            continue;
        };
        let calls: Vec<&ai::ToolCall> = assistant
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            continue;
        }
        let mut results: HashMap<&str, usize> = HashMap::new();
        for (next, candidate) in messages.iter().enumerate().skip(index + 1) {
            match candidate {
                Message::Assistant(_) => break,
                Message::ToolResult(result) => {
                    results.entry(result.tool_call_id.as_str()).or_insert(next);
                }
                _ => {}
            }
        }
        for call in calls {
            match results.get(call.id.as_str()) {
                Some(found) => ordered.push(messages[*found].clone()),
                None => ordered.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: vec![UserContent::Text(TextContent {
                        text: MISSING_RESULT_TEXT.into(),
                        text_signature: None,
                    })],
                    details: Some(json!({ "reason": "missing_result" })),
                    usage: None,
                    is_error: true,
                    timestamp: assistant.timestamp,
                })),
            }
        }
    }
    ordered
}
