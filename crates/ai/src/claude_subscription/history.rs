//! Ultron's canonical transcript as Claude Code stream-json frames, and the generation fields Claude Code receives
//! through `CLAUDE_CODE_EXTRA_BODY` (`prepare_history`, `request_body` and `projection` of `directsdk.py`).
//!
//! Earlier user frames are replayed with `shouldQuery: false` (Claude Code acknowledges each with a zero-turn
//! result); the last frame queries. An assistant message this provider produced carries its native assistant
//! messages (signed thinking included) in a versioned carrier in `providerMetadata`; while the message's visible
//! projection (text and tool calls) is unchanged the native blocks replay verbatim, otherwise canonical text and
//! `tool_use` blocks built from the message replay and the stale signatures are dropped.

use super::ClaudeError;
use super::catalog::{accepts_thinking_disable, supports_adaptive_thinking};
use crate::transcript::render_system_message_update;
use crate::types::{AssistantContent, AssistantMessage, Message, Tool, UserContent, UserMessageContent};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// Claude Code's MCP namespace for the inert inventory server named `ultron`.
pub const PREFIX: &str = "mcp__ultron__";
/// The model's one tool. Nothing else is ever advertised.
pub const TOOL: &str = "rlm";
/// `providerMetadata` key and carrier type of an assistant message this provider produced.
pub const CARRIER_KEY: &str = "claude-subscription";
pub const CARRIER: &str = "claude-subscription.native_assistant";
pub const CARRIER_VERSION: u64 = 1;

/// Reasoning as the native request expresses it: enabled or disabled, and an effort.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reasoning {
    pub enabled: Option<bool>,
    pub effort: Option<String>,
}

/// The generation fields of one request.
#[derive(Clone, Debug, Default)]
pub struct BodyParams<'a> {
    pub model: &'a str,
    pub tools: &'a [Tool],
    pub max_tokens: Option<u64>,
    pub reasoning: Option<Reasoning>,
    pub json_schema: Option<Value>,
    pub stop_sequences: Option<Vec<String>>,
}

/// The encoded extra body, the inert MCP manifest and the advertised (host) tool names.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    pub encoded: String,
    pub manifest: Vec<Value>,
    pub names: BTreeSet<String>,
}

impl Body {
    pub fn value(&self) -> Value {
        serde_json::from_str(&self.encoded).unwrap_or(Value::Null)
    }
}

const EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];

/// The native effort for a requested level: `minimal` is native `low`, `ultra` is `max`.
pub fn clamp_effort(effort: Option<&str>) -> Result<Option<String>, ClaudeError> {
    let Some(effort) = effort else { return Ok(None) };
    let mapped = match effort {
        "minimal" => "low",
        "ultra" => "max",
        other => other,
    };
    if EFFORTS.contains(&mapped) {
        Ok(Some(mapped.to_string()))
    } else {
        Err(ClaudeError::Invalid("Unsupported native reasoning effort".into()))
    }
}

/// Drop the `null` branch of nullable unions (and `null` from type lists) everywhere in `schema`.
fn strip_nullable_unions(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(strip_nullable_unions).collect()),
        Value::Object(map) => {
            let mut out: Map<String, Value> = map.iter().map(|(k, v)| (k.clone(), strip_nullable_unions(v))).collect();
            if let Some(Value::Array(types)) = out.get("type") {
                let kept: Vec<Value> = types.iter().filter(|t| *t != "null").cloned().collect();
                if kept.len() < types.len() {
                    let replaced = if kept.len() == 1 {
                        kept[0].clone()
                    } else {
                        Value::Array(kept)
                    };
                    out.insert("type".into(), replaced);
                }
            }
            for key in ["anyOf", "oneOf"] {
                let Some(Value::Array(branches)) = out.get(key) else {
                    continue;
                };
                let is_null = |b: &Value| {
                    b.get("type").is_some_and(|t| t == "null") && b.as_object().is_some_and(|m| m.len() == 1)
                };
                let kept: Vec<Value> = branches.iter().filter(|b| !is_null(b)).cloned().collect();
                if kept.len() == branches.len() || kept.is_empty() {
                    continue;
                }
                if kept.len() == 1 {
                    out.shift_remove(key);
                    if let Value::Object(single) = &kept[0] {
                        for (k, v) in single {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                } else {
                    out.insert(key.into(), Value::Array(kept));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// What Anthropic's validator accepts as a tool's input schema: no nullable union branches, no top-level
/// `oneOf`/`allOf`/`anyOf`, and an object always has `properties`. The inert manifest and the request body carry
/// the same normalized schema.
pub fn normalize_input_schema(schema: &Value) -> Value {
    let mut normalized = strip_nullable_unions(schema);
    if let Value::Object(map) = &mut normalized {
        let banned = ["oneOf", "allOf", "anyOf"];
        if banned.iter().any(|k| map.contains_key(*k)) {
            for key in banned {
                map.shift_remove(key);
            }
            map.entry("type").or_insert_with(|| json!("object"));
        }
        if map.get("type") == Some(&json!("object")) && !map.get("properties").is_some_and(Value::is_object) {
            map.insert("properties".into(), json!({}));
        }
    }
    normalized
}

/// The extra body and the inert manifest (`request_body`). The only tool that may be advertised is `rlm`.
pub fn request_body(params: &BodyParams<'_>) -> Result<Body, ClaudeError> {
    let invalid = |message: &str| ClaudeError::Invalid(message.to_string());
    let mut body = Map::new();
    if let Some(reasoning) = &params.reasoning {
        let effort = clamp_effort(reasoning.effort.as_deref())?;
        if reasoning.enabled == Some(false) || effort.as_deref() == Some("none") {
            if accepts_thinking_disable(params.model) {
                body.insert("thinking".into(), json!({"type": "disabled"}));
                // Native clear-thinking context edits are invalid when thinking is disabled.
                body.insert("context_management".into(), json!({"edits": []}));
            }
        } else {
            // Routes without adaptive thinking (Haiku 4.5) 400 on the block; the effort still applies.
            if reasoning.enabled == Some(true) && supports_adaptive_thinking(params.model) {
                body.insert("thinking".into(), json!({"type": "adaptive"}));
            }
            if let Some(effort) = effort {
                body.insert("output_config".into(), json!({"effort": effort}));
            }
        }
    }
    if let Some(schema) = &params.json_schema {
        if !schema.is_object() {
            return Err(invalid("response_format requires a JSON Schema object"));
        }
        let config = body.entry("output_config").or_insert_with(|| json!({}));
        config["format"] = json!({"type": "json_schema", "schema": schema});
    }
    if let Some(max_tokens) = params.max_tokens {
        if max_tokens < 1 {
            return Err(invalid("max_tokens must be a positive integer"));
        }
        body.insert("max_tokens".into(), json!(max_tokens));
    }
    if let Some(stop) = &params.stop_sequences {
        if stop.iter().any(String::is_empty) {
            return Err(invalid("stop_sequences must be a list of nonempty strings"));
        }
        body.insert("stop_sequences".into(), json!(stop));
    }
    let mut manifest = Vec::new();
    let mut tools = Vec::new();
    let mut names = BTreeSet::new();
    for tool in params.tools {
        if tool.name != TOOL {
            return Err(ClaudeError::Invalid(format!(
                "claude-subscription advertises exactly one tool, `{TOOL}`; refusing to advertise `{}`",
                tool.name
            )));
        }
        if !names.insert(tool.name.clone()) {
            return Err(invalid("Tool names must be unique"));
        }
        if !tool.parameters.is_object() {
            return Err(invalid("Tool schema must be an object and description a string"));
        }
        let schema = normalize_input_schema(&tool.parameters);
        manifest.push(json!({"name": tool.name, "description": tool.description, "inputSchema": schema}));
        tools.push(
            json!({"name": format!("{PREFIX}{}", tool.name), "description": tool.description, "input_schema": schema}),
        );
    }
    body.insert("tools".into(), Value::Array(tools));
    let encoded = serde_json::to_string(&Value::Object(body)).map_err(|e| ClaudeError::Invalid(e.to_string()))?;
    Ok(Body {
        encoded,
        manifest,
        names,
    })
}

/// The visible part of an assistant message the carrier is checked against: its text (trimmed) and tool calls.
pub fn projection(message: &AssistantMessage) -> Value {
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    let calls: Vec<Value> = message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => {
                Some(json!({"id": call.id, "name": call.name, "input": call.arguments}))
            }
            _ => None,
        })
        .collect();
    json!({"content": text.trim(), "tool_calls": calls})
}

/// The carrier of an assistant message this provider produced.
pub fn carrier(messages: Vec<Value>, projection: Value) -> Value {
    json!({"type": CARRIER, "version": CARRIER_VERSION, "messages": messages, "projection": projection})
}

fn user_blocks(content: &[UserContent]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            UserContent::Text(t) if t.text.is_empty() => None,
            UserContent::Text(t) => Some(json!({"type": "text", "text": t.text})),
            UserContent::Image(image) => Some(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": image.mime_type, "data": image.data},
            })),
        })
        .collect()
}

/// The native messages a carrier replays, when `message` still shows what the carrier recorded.
fn carried(message: &AssistantMessage) -> Result<Option<Vec<Value>>, ClaudeError> {
    let Some(carrier) = message.provider_metadata.as_ref().and_then(|m| m.get(CARRIER_KEY)) else {
        return Ok(None);
    };
    if carrier.get("type").and_then(Value::as_str) != Some(CARRIER)
        || carrier.get("version").and_then(Value::as_u64) != Some(CARRIER_VERSION)
    {
        return Err(ClaudeError::Invalid(
            "Unsupported native assistant carrier version".into(),
        ));
    }
    let mut expected = carrier.get("projection").cloned().unwrap_or(Value::Null);
    if let Some(text) = expected.get("content").and_then(Value::as_str) {
        expected["content"] = json!(text.trim());
    }
    if projection(message) != expected {
        // Ultron's compaction and hooks own the visible history: never restore stale blocks or attach their
        // signatures to rewritten content.
        return Ok(None);
    }
    Ok(carrier.get("messages").and_then(Value::as_array).cloned())
}

fn push(frames: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if role == "user"
        && let Some(last) = frames.last_mut()
        && last["type"] == "user"
        && let Some(content) = last["message"]["content"].as_array_mut()
    {
        content.extend(blocks);
        return;
    }
    frames.push(json!({"type": role, "message": {"role": role, "content": blocks}}));
}

/// Stream-json frames for `messages` (the transcript after the leading system messages, already transformed for
/// the target model). A later system message replays in place as user text, so the system prompt stays the
/// conversation's stable prefix. `names` are the advertised host tool names.
pub fn prepare_history(messages: &[Message], names: &BTreeSet<String>) -> Result<Vec<Value>, ClaudeError> {
    let mut frames: Vec<Value> = Vec::new();
    for message in messages {
        match message {
            Message::System(system) => {
                if frames.is_empty() {
                    continue;
                }
                let text = render_system_message_update(system);
                if !text.is_empty() {
                    push(&mut frames, "user", vec![json!({"type": "text", "text": text})]);
                }
            }
            Message::User(user) => {
                let blocks = match &user.content {
                    UserMessageContent::Text(text) if text.is_empty() => Vec::new(),
                    UserMessageContent::Text(text) => vec![json!({"type": "text", "text": text})],
                    UserMessageContent::Blocks(blocks) => user_blocks(blocks),
                };
                push(&mut frames, "user", blocks);
            }
            Message::Assistant(assistant) => {
                if let Some(native) = carried(assistant)? {
                    for message in native {
                        frames.push(json!({"type": "assistant", "message": message}));
                    }
                    continue;
                }
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(t) if !t.text.trim().is_empty() => {
                            blocks.push(json!({"type": "text", "text": t.text}));
                        }
                        AssistantContent::ToolCall(call) => {
                            // A call outside the inventory replays as the model named it.
                            let name = if names.contains(&call.name) {
                                format!("{PREFIX}{}", call.name)
                            } else {
                                call.name.clone()
                            };
                            let input = if call.arguments.is_object() {
                                call.arguments.clone()
                            } else {
                                json!({})
                            };
                            blocks.push(json!({"type": "tool_use", "id": call.id, "name": name, "input": input}));
                        }
                        _ => {}
                    }
                }
                push(&mut frames, "assistant", blocks);
            }
            Message::ToolResult(result) => {
                let mut block = json!({"type": "tool_result", "tool_use_id": result.tool_call_id});
                match &result.content[..] {
                    [] => {}
                    [UserContent::Text(t)] => block["content"] = json!(t.text),
                    content => block["content"] = Value::Array(user_blocks(content)),
                }
                if result.is_error {
                    block["is_error"] = json!(true);
                }
                push(&mut frames, "user", vec![block]);
            }
        }
    }
    let ends_in_user = frames
        .last()
        .is_some_and(|f| f["type"] == "user" && f["message"]["content"].as_array().is_some_and(|c| !c.is_empty()));
    if !ends_in_user {
        return Err(ClaudeError::Invalid(
            "History must end in a nonempty user/tool-result message; assistant prefill is unsupported".into(),
        ));
    }
    Ok(frames)
}
