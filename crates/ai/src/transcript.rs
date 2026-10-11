//! Transcript helpers (`utils/transcript.ts`, `utils/text.ts`), cross-provider message transformation
//! (`api/transform-messages.ts`) and the context token estimate (`utils/estimate.ts`).

use crate::json::{js_len, stringify};
use crate::types::{
    AssistantContent, AssistantMessage, Context, Message, Model, StopReason, SystemContent, SystemMessage, TextContent,
    Tool, ToolCall, ToolResultMessage, TranscriptContext, TypedText, UserContent, UserMessageContent,
};
use indexmap::IndexMap;
use std::collections::HashSet;

pub fn system_content_text(content: &SystemContent) -> String {
    match content {
        SystemContent::Text(text) => text.clone(),
        SystemContent::Blocks(blocks) => blocks
            .iter()
            .map(|TypedText::Text(t)| t.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// The leading prompt: content followed by its sections.
pub fn system_message_text(message: &SystemMessage) -> String {
    let mut parts = vec![system_content_text(&message.content)];
    for text in message.sections.iter().flatten().filter_map(|(_, v)| v.clone()) {
        parts.push(text);
    }
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A later system message rendered in place, section changes framed by name.
pub fn render_system_message_update(message: &SystemMessage) -> String {
    let mut parts = Vec::new();
    let text = system_content_text(&message.content);
    if !text.is_empty() {
        parts.push(text);
    }
    for (name, value) in message.sections.iter().flatten() {
        parts.push(match value {
            None => format!("Removed system prompt section \"{name}\"."),
            Some(value) => format!("Updated system prompt section \"{name}\":\n\n{value}"),
        });
    }
    parts.join("\n\n")
}

fn systems(messages: &[Message]) -> impl Iterator<Item = &SystemMessage> {
    messages.iter().filter_map(|m| match m {
        Message::System(s) => Some(s),
        _ => None,
    })
}

pub fn create_initial_system_message(system_prompt: Option<&str>, tools: Option<&[Tool]>) -> Option<SystemMessage> {
    let has_prompt = system_prompt.is_some_and(|p| !p.is_empty());
    let has_tools = tools.is_some_and(|t| !t.is_empty());
    if !has_prompt && !has_tools {
        return None;
    }
    Some(SystemMessage {
        content: SystemContent::Text(system_prompt.unwrap_or_default().to_string()),
        sections: None,
        tools_added: if has_tools { tools.map(<[Tool]>::to_vec) } else { None },
        tools_removed: None,
        timestamp: 0,
    })
}

pub fn normalize_context(context: &Context) -> TranscriptContext {
    let initial = create_initial_system_message(context.system_prompt.as_deref(), context.tools.as_deref());
    let mut messages: Vec<Message> = initial.into_iter().map(Message::System).collect();
    messages.extend(context.messages.iter().cloned());
    TranscriptContext { messages }
}

pub fn initial_system_message(messages: &[Message]) -> Option<&SystemMessage> {
    match messages.first() {
        Some(Message::System(s)) => Some(s),
        _ => None,
    }
}

pub fn current_tools(messages: &[Message]) -> Vec<Tool> {
    let mut tools: IndexMap<String, Tool> = IndexMap::new();
    for message in systems(messages) {
        for tool in message.tools_removed.iter().flatten() {
            tools.shift_remove(&tool.name);
        }
        for tool in message.tools_added.iter().flatten() {
            tools.insert(tool.name.clone(), tool.clone());
        }
    }
    tools.into_values().collect()
}

pub fn current_system_message(messages: &[Message]) -> Option<SystemMessage> {
    let mut content = Vec::new();
    let mut sections: IndexMap<String, String> = IndexMap::new();
    let mut timestamp = None;
    for message in systems(messages) {
        timestamp.get_or_insert(message.timestamp);
        let text = system_content_text(&message.content);
        if !text.is_empty() {
            content.push(text);
        }
        for (name, value) in message.sections.iter().flatten() {
            match value {
                None => {
                    sections.shift_remove(name);
                }
                Some(value) => {
                    sections.insert(name.clone(), value.clone());
                }
            }
        }
    }
    let tools = current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: SystemContent::Text(content.join("\n\n")),
        sections: (!sections.is_empty()).then(|| sections.into_iter().map(|(k, v)| (k, Some(v))).collect()),
        tools_added: (!tools.is_empty()).then_some(tools),
        tools_removed: None,
        timestamp: timestamp.unwrap_or(0),
    })
}

pub fn collapse_system_messages(context: &TranscriptContext) -> TranscriptContext {
    let head = current_system_message(&context.messages);
    let mut messages: Vec<Message> = head.into_iter().map(Message::System).collect();
    messages.extend(
        context
            .messages
            .iter()
            .filter(|m| !matches!(m, Message::System(_)))
            .cloned(),
    );
    TranscriptContext { messages }
}

pub fn resolve_transcript(context: &TranscriptContext, supports_mid_convo_system_messages: bool) -> TranscriptContext {
    if supports_mid_convo_system_messages {
        context.clone()
    } else {
        collapse_system_messages(context)
    }
}

/// The declaration a tool exposes to the model, serialized for comparison.
fn declaration(tool: &Tool) -> String {
    serde_json::to_string(tool).unwrap_or_default()
}

pub fn declared_tools(messages: &[Message]) -> Vec<Tool> {
    let mut definitions: IndexMap<String, Tool> = IndexMap::new();
    for message in systems(messages) {
        for tool in message.tools_added.iter().flatten() {
            definitions.insert(tool.name.clone(), tool.clone());
        }
    }
    definitions.into_values().collect()
}

pub fn has_tool_redefinitions(messages: &[Message]) -> bool {
    let mut declared: IndexMap<&str, String> = IndexMap::new();
    for message in systems(messages) {
        for tool in message.tools_added.iter().flatten() {
            let current = declaration(tool);
            if declared
                .get(tool.name.as_str())
                .is_some_and(|previous| *previous != current)
            {
                return true;
            }
            declared.insert(&tool.name, current);
        }
    }
    false
}

pub fn has_non_additive_tool_changes(messages: &[Message]) -> bool {
    let mut declared = HashSet::new();
    for message in systems(messages) {
        if message.tools_removed.as_ref().is_some_and(|r| !r.is_empty()) {
            return true;
        }
        for tool in message.tools_added.iter().flatten() {
            if !declared.insert(tool.name.as_str()) {
                return true;
            }
        }
    }
    false
}

pub struct TranscriptTools {
    pub request_tools: Vec<Tool>,
    pub anchors_additions: bool,
}

pub fn resolve_transcript_tools(messages: &[Message], supports_tool_additions: bool) -> TranscriptTools {
    let anchors_additions = supports_tool_additions && !has_non_additive_tool_changes(messages);
    let request_tools = if anchors_additions {
        initial_system_message(messages)
            .and_then(|m| m.tools_added.clone())
            .unwrap_or_default()
    } else {
        current_tools(messages)
    };
    TranscriptTools {
        request_tools,
        anchors_additions,
    }
}

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";

fn replace_images(content: &[UserContent], placeholder: &str) -> Vec<UserContent> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            UserContent::Image(_) => {
                if !previous_was_placeholder {
                    result.push(UserContent::Text(TextContent {
                        text: placeholder.into(),
                        text_signature: None,
                    }));
                }
                previous_was_placeholder = true;
            }
            UserContent::Text(text) => {
                previous_was_placeholder = text.text == placeholder;
                result.push(block.clone());
            }
        }
    }
    result
}

/// Callback normalizing a foreign tool call id for the target model.
pub type NormalizeToolCallId<'a> = &'a dyn Fn(&str, &AssistantMessage) -> String;

/// Prepare a transcript for `model`: downgrade images for text-only models, convert foreign thinking to text, drop
/// errored turns, normalize tool call ids, and insert "No result provided" results for orphaned tool calls.
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize: Option<NormalizeToolCallId<'_>>,
    now: i64,
) -> Vec<Message> {
    let mut id_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let supports_images = model.supports_images();
    let mut transformed = Vec::with_capacity(messages.len());
    for message in messages {
        let message = match message {
            Message::User(user) if !supports_images => match &user.content {
                UserMessageContent::Blocks(blocks) => {
                    let mut user = user.clone();
                    user.content =
                        UserMessageContent::Blocks(replace_images(blocks, NON_VISION_USER_IMAGE_PLACEHOLDER));
                    Message::User(user)
                }
                UserMessageContent::Text(_) => message.clone(),
            },
            Message::ToolResult(result) => {
                let mut result = result.clone();
                if !supports_images {
                    result.content = replace_images(&result.content, NON_VISION_TOOL_IMAGE_PLACEHOLDER);
                }
                if let Some(id) = id_map.get(&result.tool_call_id) {
                    result.tool_call_id = id.clone();
                }
                Message::ToolResult(result)
            }
            Message::Assistant(assistant) => {
                let same_model =
                    assistant.provider == model.provider && assistant.api == model.api && assistant.model == model.id;
                let mut content = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if thinking.redacted == Some(true) {
                                if same_model {
                                    content.push(block.clone());
                                }
                            } else if same_model && thinking.thinking_signature.as_ref().is_some_and(|s| !s.is_empty())
                            {
                                content.push(block.clone());
                            } else if thinking.thinking.trim().is_empty() {
                            } else if same_model {
                                content.push(block.clone());
                            } else {
                                content.push(AssistantContent::Text(TextContent {
                                    text: thinking.thinking.clone(),
                                    text_signature: None,
                                }));
                            }
                        }
                        AssistantContent::Text(text) => content.push(if same_model {
                            block.clone()
                        } else {
                            AssistantContent::Text(TextContent {
                                text: text.text.clone(),
                                text_signature: None,
                            })
                        }),
                        AssistantContent::ToolCall(call) => {
                            let mut call: ToolCall = call.clone();
                            if !same_model {
                                call.thought_signature = None;
                                if let Some(normalize) = normalize {
                                    let id = normalize(&call.id, assistant);
                                    if id != call.id {
                                        id_map.insert(call.id.clone(), id.clone());
                                        call.id = id;
                                    }
                                }
                            }
                            content.push(AssistantContent::ToolCall(call));
                        }
                    }
                }
                let mut assistant = assistant.clone();
                assistant.content = content;
                Message::Assistant(assistant)
            }
            other => other.clone(),
        };
        transformed.push(message);
    }

    let mut result = Vec::with_capacity(transformed.len());
    let mut pending: Vec<ToolCall> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();
    let mut held: Vec<Message> = Vec::new();
    let close = |result: &mut Vec<Message>,
                 pending: &mut Vec<ToolCall>,
                 answered: &mut HashSet<String>,
                 held: &mut Vec<Message>| {
        for call in pending.drain(..) {
            if !answered.contains(&call.id) {
                result.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: call.id,
                    tool_name: call.name,
                    content: vec![UserContent::Text(TextContent {
                        text: "No result provided".into(),
                        text_signature: None,
                    })],
                    details: None,
                    usage: None,
                    is_error: true,
                    timestamp: now,
                }));
            }
        }
        answered.clear();
        result.append(held);
    };
    for message in transformed {
        match &message {
            Message::Assistant(assistant) => {
                close(&mut result, &mut pending, &mut answered, &mut held);
                if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                    continue;
                }
                let calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantContent::ToolCall(c) => Some(c.clone()),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    pending = calls;
                    answered.clear();
                }
                result.push(message);
            }
            Message::ToolResult(tool_result) => {
                answered.insert(tool_result.tool_call_id.clone());
                result.push(message);
            }
            Message::System(_) if !pending.is_empty() => held.push(message),
            Message::System(_) => result.push(message),
            Message::User(_) => {
                close(&mut result, &mut pending, &mut answered, &mut held);
                result.push(message);
            }
        }
    }
    close(&mut result, &mut pending, &mut answered, &mut held);
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

const CHARS_PER_TOKEN: usize = 4;
const ESTIMATED_IMAGE_CHARS: usize = 4800;

fn ceil_tokens(chars: usize) -> u64 {
    chars.div_ceil(CHARS_PER_TOKEN) as u64
}

fn user_content_chars(content: &[UserContent]) -> usize {
    content
        .iter()
        .map(|b| match b {
            UserContent::Text(t) => js_len(&t.text),
            UserContent::Image(_) => ESTIMATED_IMAGE_CHARS,
        })
        .sum()
}

pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::System(system) => {
            let tools = |json: Option<String>| json.map_or(0, |j| ceil_tokens(js_len(&j)));
            ceil_tokens(js_len(&system_message_text(system)))
                + tools(
                    system
                        .tools_added
                        .as_ref()
                        .filter(|t| !t.is_empty())
                        .map(|t| serde_json::to_string(t).unwrap_or_default()),
                )
                + tools(
                    system
                        .tools_removed
                        .as_ref()
                        .filter(|t| !t.is_empty())
                        .map(|t| serde_json::to_string(t).unwrap_or_default()),
                )
        }
        Message::User(user) => match &user.content {
            UserMessageContent::Text(text) => ceil_tokens(js_len(text)),
            UserMessageContent::Blocks(blocks) => ceil_tokens(user_content_chars(blocks)),
        },
        Message::ToolResult(result) => ceil_tokens(user_content_chars(&result.content)),
        Message::Assistant(assistant) => ceil_tokens(
            assistant
                .content
                .iter()
                .map(|b| match b {
                    AssistantContent::Text(t) => js_len(&t.text),
                    AssistantContent::Thinking(t) => js_len(&t.thinking),
                    AssistantContent::ToolCall(c) => js_len(&c.name) + js_len(&stringify(&c.arguments)),
                })
                .sum(),
        ),
    }
}

fn context_tokens(usage: &crate::types::Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

pub fn estimate_context_tokens(messages: &[Message]) -> ContextUsageEstimate {
    let mut latest_prefix = i64::MIN;
    let mut usage_info = None;
    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message
            && assistant.timestamp >= latest_prefix
            && !matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error)
            && context_tokens(&assistant.usage) > 0
        {
            usage_info = Some((context_tokens(&assistant.usage), index));
        }
        latest_prefix = latest_prefix.max(message.timestamp());
    }
    if let Some((usage_tokens, index)) = usage_info {
        let trailing: u64 = messages[index + 1..].iter().map(estimate_message_tokens).sum();
        return ContextUsageEstimate {
            tokens: usage_tokens + trailing,
            usage_tokens,
            trailing_tokens: trailing,
            last_usage_index: Some(index),
        };
    }
    let tokens = messages.iter().map(estimate_message_tokens).sum();
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}
