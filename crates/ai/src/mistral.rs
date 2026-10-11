//! `api/mistral-conversations.ts`: the native Mistral Chat Completions provider (direct `fetch`, no SDK): payload and
//! wire-key remapping, tool call id normalization to Mistral's 9-character ids, the SSE reader, and stream handling.
//! Evidence: `fixtures/generate-more.mjs` records the TypeScript provider; `tests/providers_more.rs` replays it.

use crate::constrained::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::http::{HeaderBuilder, Read, cancelled, is_aborted, read_chunk, truncate_error_text};
use crate::json::{parse_json, parse_streaming_json, short_hash, stringify};
use crate::models::{calculate_cost, clamp_thinking_level};
use crate::options::{SimpleStreamOptions, StreamOptions, build_base_options, now_ms, pi_user_agent};
use crate::transcript::{
    current_tools, render_system_message_update, resolve_transcript, system_message_text, transform_messages,
};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, EventSink, Mapped, Message, Model,
    StopReason, TextContent, ThinkingContent, Tool, ToolCall, TranscriptContext, UserContent, UserMessageContent,
};
use crate::{HttpRequest, HttpResponse, Transport};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;

const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;
const MAX_MISTRAL_ERROR_BODY_CHARS: usize = 4000;

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MistralOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    /// `"auto" | "none" | "any" | "required"` or `{type: "function", function: {name}}`.
    pub tool_choice: Option<Value>,
    /// `"reasoning"`.
    pub prompt_mode: Option<String>,
    /// `"none" | "high"`.
    pub reasoning_effort: Option<String>,
}

/// `deriveMistralToolCallId`: 9 alphanumerics, kept when the id already is, else hashed.
pub fn derive_mistral_tool_call_id(id: &str, attempt: u32) -> String {
    let normalized: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() { id } else { &normalized };
    let seed = if attempt == 0 {
        seed_base.to_string()
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

/// `createMistralToolCallIdNormalizer`: stable ids, re-derived on collision.
#[derive(Default)]
struct IdNormalizer {
    ids: HashMap<String, String>,
    owners: HashMap<String, String>,
}

impl IdNormalizer {
    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.ids.get(id) {
            return existing.clone();
        }
        let mut attempt = 0;
        loop {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            if self.owners.get(&candidate).is_none_or(|owner| owner == id) {
                self.ids.insert(id.to_string(), candidate.clone());
                self.owners.insert(candidate.clone(), id.to_string());
                return candidate;
            }
            attempt += 1;
        }
    }
}

fn uses_prompt_caching(options: &StreamOptions) -> Option<&str> {
    if options.cache_retention == Some(CacheRetention::None) {
        return None;
    }
    options.session_id.as_deref().filter(|s| !s.is_empty())
}

fn image_url(mime_type: &str, data: &str) -> Value {
    json!({ "type": "image_url", "imageUrl": format!("data:{mime_type};base64,{data}") })
}

fn tool_result_text(text: &str, has_images: bool, supports_images: bool, is_error: bool) -> String {
    let trimmed = text.trim();
    let prefix = if is_error { "[tool error] " } else { "" };
    if !trimmed.is_empty() {
        let suffix = if has_images && !supports_images {
            "\n[tool image omitted: model does not support images]"
        } else {
            ""
        };
        return format!("{prefix}{trimmed}{suffix}");
    }
    if has_images {
        let body = if supports_images {
            "(see attached image)"
        } else {
            "(image omitted: model does not support images)"
        };
        return format!("{prefix}{body}");
    }
    format!("{prefix}(no tool output)")
}

/// `toChatMessages`, in the payload's camelCase shape (`toolCalls`, `toolCallId`, `imageUrl`).
fn to_chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut result = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::System(system) => {
                let text = if index == 0 {
                    system_message_text(system)
                } else {
                    render_system_message_update(system)
                };
                if !text.is_empty() {
                    result.push(json!({ "role": "system", "content": text }));
                }
            }
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => result.push(json!({ "role": "user", "content": text })),
                UserMessageContent::Blocks(blocks) => {
                    let had_images = blocks.iter().any(|b| matches!(b, UserContent::Image(_)));
                    let content: Vec<Value> = blocks
                        .iter()
                        .filter_map(|block| match block {
                            UserContent::Text(t) => Some(json!({ "type": "text", "text": t.text })),
                            UserContent::Image(i) if supports_images => Some(image_url(&i.mime_type, &i.data)),
                            UserContent::Image(_) => None,
                        })
                        .collect();
                    if !content.is_empty() {
                        result.push(json!({ "role": "user", "content": content }));
                    } else if had_images && !supports_images {
                        result.push(
                            json!({ "role": "user", "content": "(image omitted: model does not support images)" }),
                        );
                    }
                }
            },
            Message::Assistant(assistant) => {
                let mut content = Vec::new();
                let mut tool_calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(t) if !t.text.trim().is_empty() => {
                            content.push(json!({ "type": "text", "text": t.text }));
                        }
                        AssistantContent::Thinking(t) if !t.thinking.trim().is_empty() => {
                            content.push(
                                json!({ "type": "thinking", "thinking": [{ "type": "text", "text": t.thinking }] }),
                            );
                        }
                        AssistantContent::ToolCall(call) => {
                            let arguments = if js_truthy(&call.arguments) {
                                call.arguments.clone()
                            } else {
                                json!({})
                            };
                            tool_calls.push(json!({
                                "id": call.id,
                                "type": "function",
                                "function": { "name": call.name, "arguments": stringify(&arguments) },
                                "index": 0,
                            }));
                        }
                        _ => {}
                    }
                }
                if content.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut wire = Map::new();
                wire.insert("role".into(), "assistant".into());
                wire.insert("prefix".into(), false.into());
                if !content.is_empty() {
                    wire.insert("content".into(), Value::Array(content));
                }
                if !tool_calls.is_empty() {
                    wire.insert("toolCalls".into(), Value::Array(tool_calls));
                }
                result.push(Value::Object(wire));
            }
            Message::ToolResult(tool) => {
                let text = tool
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        UserContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool.content.iter().any(|c| matches!(c, UserContent::Image(_)));
                let mut content = vec![json!({
                    "type": "text",
                    "text": tool_result_text(&text, has_images, supports_images, tool.is_error),
                })];
                if supports_images {
                    content.extend(tool.content.iter().filter_map(|c| match c {
                        UserContent::Image(i) => Some(image_url(&i.mime_type, &i.data)),
                        UserContent::Text(_) => None,
                    }));
                }
                result.push(json!({
                    "role": "tool",
                    "toolCallId": tool.tool_call_id,
                    "name": tool.tool_name,
                    "content": content,
                }));
            }
        }
    }
    result
}

fn js_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false)) && value.as_str() != Some("") && value.as_f64() != Some(0.0)
}

fn to_function_tools(tools: &[Tool]) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let strict = resolve_json_schema_strict_sampling(tool, true)?;
            Ok(json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": json_schema_tool_parameters(tool, strict)?,
                    "strict": strict.unwrap_or(false),
                },
            }))
        })
        .collect()
}

/// `mapToolChoice`: a known string passes through, anything else names a function.
fn map_tool_choice(choice: &Value) -> Option<Value> {
    if !js_truthy(choice) {
        return None;
    }
    if let Some(s @ ("auto" | "none" | "any" | "required")) = choice.as_str() {
        return Some(s.into());
    }
    let name = choice.pointer("/function/name").cloned().unwrap_or(Value::Null);
    let mut function = Map::new();
    if !name.is_null() {
        function.insert("name".into(), name);
    }
    Some(json!({ "type": "function", "function": function }))
}

/// `buildChatPayload` (camelCase payload; `onPayload` sees this shape).
pub fn build_payload(
    model: &Model,
    context: &TranscriptContext,
    messages: &[Message],
    options: &MistralOptions,
) -> Result<Value, String> {
    let mut payload = Map::new();
    payload.insert("model".into(), model.id.clone().into());
    payload.insert("stream".into(), true.into());
    payload.insert(
        "messages".into(),
        Value::Array(to_chat_messages(messages, model.supports_images())),
    );
    let tools = current_tools(&context.messages);
    if !tools.is_empty() {
        payload.insert("tools".into(), Value::Array(to_function_tools(&tools)?));
    }
    if let Some(t) = options.base.temperature {
        payload.insert("temperature".into(), json!(t));
    }
    if let Some(m) = options.base.max_tokens {
        payload.insert("maxTokens".into(), json!(m));
    }
    if let Some(choice) = options.tool_choice.as_ref().and_then(map_tool_choice) {
        payload.insert("toolChoice".into(), choice);
    }
    if let Some(mode) = options.prompt_mode.as_ref().filter(|m| !m.is_empty()) {
        payload.insert("promptMode".into(), mode.clone().into());
    }
    if let Some(effort) = options.reasoning_effort.as_ref().filter(|e| !e.is_empty()) {
        payload.insert("reasoningEffort".into(), effort.clone().into());
    }
    if let Some(session) = uses_prompt_caching(&options.base) {
        payload.insert("promptCacheKey".into(), session.into());
    }
    Ok(Value::Object(payload))
}

/// `remapMistralProperty`: the value moves to `target` (appended unless already present), `source` is deleted.
fn remap(record: &mut Map<String, Value>, source: &str, target: &str) {
    if let Some(value) = record.shift_remove(source) {
        // Assigning an existing key keeps its position, as in JS.
        record.insert(target.into(), value);
    }
}

/// `toMistralWirePayload`: camelCase SDK names to the snake_case wire names.
pub fn to_wire_payload(payload: &Value) -> Value {
    let mut wire = payload.as_object().cloned().unwrap_or_default();
    for (source, target) in [
        ("topP", "top_p"),
        ("maxTokens", "max_tokens"),
        ("randomSeed", "random_seed"),
        ("responseFormat", "response_format"),
        ("toolChoice", "tool_choice"),
        ("presencePenalty", "presence_penalty"),
        ("frequencyPenalty", "frequency_penalty"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("reasoningEffort", "reasoning_effort"),
        ("promptMode", "prompt_mode"),
        ("promptCacheKey", "prompt_cache_key"),
        ("safePrompt", "safe_prompt"),
    ] {
        remap(&mut wire, source, target);
    }
    if let Some(Value::Array(messages)) = payload.get("messages") {
        let messages = messages
            .iter()
            .map(|message| {
                let mut m = message.as_object().cloned().unwrap_or_default();
                remap(&mut m, "toolCalls", "tool_calls");
                remap(&mut m, "toolCallId", "tool_call_id");
                if let Some(Value::Array(content)) = m.get_mut("content") {
                    for chunk in content.iter_mut().filter_map(Value::as_object_mut) {
                        for (source, target) in [
                            ("imageUrl", "image_url"),
                            ("documentUrl", "document_url"),
                            ("documentName", "document_name"),
                            ("fileId", "file_id"),
                            ("referenceIds", "reference_ids"),
                            ("inputAudio", "input_audio"),
                        ] {
                            remap(chunk, source, target);
                        }
                    }
                }
                Value::Object(m)
            })
            .collect();
        wire.insert("messages".into(), Value::Array(messages));
    }
    if let Some(Value::Object(format)) = wire.get_mut("response_format") {
        remap(format, "jsonSchema", "json_schema");
        if let Some(Value::Object(schema)) = format.get_mut("json_schema") {
            remap(schema, "schemaDefinition", "schema");
        }
    }
    Value::Object(wire)
}

/// `new URL("v1/chat/completions", base + "/")`: the base path keeps one trailing slash; query and fragment drop.
fn chat_url(base_url: &str) -> String {
    let base = base_url.split(['?', '#']).next().unwrap_or_default();
    let (origin, path) = match base.find("://").map(|i| i + 3) {
        Some(start) => match base[start..].find('/') {
            Some(slash) => base.split_at(start + slash),
            None => (base, ""),
        },
        None => (base, ""),
    };
    format!("{origin}{}/v1/chat/completions", path.trim_end_matches('/'))
}

/// A failed request or stream: the `MistralHttpError` fields, or a plain error message.
struct MistralError {
    message: String,
    status: Option<u16>,
    body: Option<String>,
}

impl MistralError {
    fn plain(message: impl Into<String>) -> Self {
        MistralError {
            message: message.into(),
            status: None,
            body: None,
        }
    }

    /// `formatMistralError`.
    fn format(&self) -> String {
        let body = self.body.as_deref().map(str::trim).filter(|b| !b.is_empty());
        match (self.status, body) {
            (Some(status), Some(body)) => format!(
                "Mistral API error ({status}): {}",
                truncate_error_text(body, MAX_MISTRAL_ERROR_BODY_CHARS)
            ),
            (Some(status), None) => format!("Mistral API error ({status}): {}", self.message),
            _ => self.message.clone(),
        }
    }
}

const ABORTED: &str = "This operation was aborted";

/// Stream one Mistral response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &MistralOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let context = resolve_transcript(
        context,
        model.compat_bool("supportsMidConvoSystemMessages").unwrap_or(false),
    );
    let mut run = Run {
        output: AssistantMessage::pending(model, now_ms()),
        sink,
        current: None,
        tools: IndexMap::new(),
    };
    let signal = options.base.signal();
    match drive(&mut run, model, &context, options, transport).await {
        Ok(()) => {
            let reason = run.output.stop_reason;
            run.emit(|message| AssistantMessageEvent::Done { reason, message });
        }
        Err(error) => {
            run.output.stop_reason = if is_aborted(signal) {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            run.output.error_message = Some(error.format());
            let reason = run.output.stop_reason;
            run.emit(|error| AssistantMessageEvent::Error { reason, error });
        }
    }
    run.output
}

async fn drive(
    run: &mut Run<'_, '_>,
    model: &Model,
    context: &TranscriptContext,
    options: &MistralOptions,
    transport: &dyn Transport,
) -> Result<(), MistralError> {
    let base = &options.base;
    let signal = base.signal();
    let api_key = base
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| MistralError::plain(format!("No API key for provider: {}", model.provider)))?;
    let normalizer = RefCell::new(IdNormalizer::default());
    let normalize = |id: &str, _: &AssistantMessage| normalizer.borrow_mut().normalize(id);
    let messages = transform_messages(&context.messages, model, Some(&normalize), run.output.timestamp);
    let mut payload = build_payload(model, context, &messages, options).map_err(MistralError::plain)?;
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&payload, model)) {
        payload = next;
    }
    let mut headers = HeaderBuilder::default();
    headers.set("User-Agent", pi_user_agent());
    headers.set("accept", "text/event-stream");
    headers.set("authorization", format!("Bearer {api_key}"));
    headers.set("content-type", "application/json");
    headers.extend_set(model.headers.iter().flatten());
    headers.extend(base.headers.iter().flatten());
    let explicit_affinity = model
        .headers
        .iter()
        .flatten()
        .any(|(k, _)| k.to_lowercase() == "x-affinity")
        || base
            .headers
            .iter()
            .flatten()
            .any(|(k, _)| k.to_lowercase() == "x-affinity");
    if let Some(session) = uses_prompt_caching(base).filter(|_| !explicit_affinity) {
        headers.set("x-affinity", session);
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: chat_url(&model.base_url),
        headers: headers.build(),
        body: serde_json::to_string(&to_wire_payload(&payload)).unwrap_or_default(),
    };
    // ponytail: upstream also bounds the request with `timeoutMs` (default 60 s); here the transport's timeout applies.
    let sent = tokio::select! {
        sent = transport.send(request) => sent,
        () = cancelled(signal) => return Err(MistralError::plain(ABORTED)),
    };
    let mut response = sent.map_err(|_| MistralError::plain("fetch failed"))?;
    if !(200..300).contains(&response.status) {
        let mut bytes = Vec::new();
        while let Ok(Some(chunk)) = response.body.next_chunk().await {
            bytes.extend_from_slice(&chunk);
        }
        return Err(MistralError {
            // ponytail: the Transport carries no reason phrase, so upstream's `statusText` fallback always applies.
            message: format!("Request failed with status {}", response.status),
            status: Some(response.status),
            body: Some(String::from_utf8_lossy(&bytes).into_owned()),
        });
    }
    run.emit(|partial| AssistantMessageEvent::Start { partial });
    read_mistral_events(&mut response, signal.cloned(), |event| run.handle(event, model)).await?;
    run.finish();
    if is_aborted(signal) {
        return Err(MistralError::plain("Request was aborted"));
    }
    match run.output.stop_reason {
        StopReason::Pending => Err(MistralError::plain("Mistral stream ended without a finish reason")),
        StopReason::Aborted | StopReason::Error => Err(MistralError::plain(
            run.output
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "An unknown error occurred".into()),
        )),
        _ => Ok(()),
    }
}

/// `findMistralEventBoundary`: the earliest blank-line separator in any newline convention.
fn event_boundary(buffer: &str) -> Option<(usize, usize)> {
    const SEPARATORS: [&str; 8] = [
        "\r\n\r\n", "\r\n\r", "\r\n\n", "\r\r\n", "\n\r\n", "\r\r", "\n\r", "\n\n",
    ];
    let bytes = buffer.as_bytes();
    (0..bytes.len()).find_map(|at| {
        SEPARATORS
            .iter()
            .find(|sep| bytes[at..].starts_with(sep.as_bytes()))
            .map(|sep| (at, sep.len()))
    })
}

enum Parsed {
    Event(Value),
    Done,
    Skip,
}

/// `parseMistralEvent`: the joined `data:` lines; `[DONE]` ends the stream; anything else must have `choices`.
fn parse_event(raw: &str) -> Result<Parsed, MistralError> {
    let data = raw
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    let data = data.trim();
    if data.is_empty() {
        return Ok(Parsed::Skip);
    }
    if data == "[DONE]" {
        return Ok(Parsed::Done);
    }
    let parsed = parse_json(data).map_err(|e| MistralError::plain(e.to_string()))?;
    if !parsed.get("choices").is_some_and(Value::is_array) {
        return Err(MistralError::plain("Invalid Mistral streaming event"));
    }
    Ok(Parsed::Event(parsed))
}

/// `readMistralEvents`: events of one read are all handled before the next abort check.
async fn read_mistral_events(
    response: &mut HttpResponse,
    signal: Option<crate::AbortSignal>,
    mut handle: impl FnMut(&Value),
) -> Result<(), MistralError> {
    let signal = signal.as_ref();
    let mut bytes: Vec<u8> = Vec::new();
    let mut buffer = String::new();
    loop {
        if is_aborted(signal) {
            return Err(MistralError::plain(ABORTED));
        }
        let done = match read_chunk(response.body.as_mut(), signal).await {
            Read::Chunk(chunk) => {
                bytes.extend_from_slice(&chunk);
                false
            }
            Read::End => true,
            Read::Aborted => return Err(MistralError::plain(ABORTED)),
            Read::Failed(error) => return Err(MistralError::plain(error)),
        };
        if is_aborted(signal) {
            return Err(MistralError::plain(ABORTED));
        }
        // Decode the complete UTF-8 prefix; an incomplete trailing character waits for the next chunk.
        let valid = match std::str::from_utf8(&bytes) {
            Ok(text) => text.len(),
            Err(error) if !done && error.error_len().is_none() => error.valid_up_to(),
            Err(_) => bytes.len(),
        };
        buffer.push_str(&String::from_utf8_lossy(&bytes[..valid]));
        bytes.drain(..valid);
        while let Some((at, len)) = event_boundary(&buffer) {
            let raw = buffer[..at].to_string();
            buffer.drain(..at + len);
            match parse_event(&raw)? {
                Parsed::Done => return Ok(()),
                Parsed::Event(event) => handle(&event),
                Parsed::Skip => {}
            }
        }
        if done {
            break;
        }
    }
    if !buffer.trim().is_empty()
        && let Parsed::Event(event) = parse_event(&buffer)?
    {
        handle(&event);
    }
    Ok(())
}

fn cached_prompt_tokens(usage: &Value, prompt_tokens: u64) -> u64 {
    let raw = [
        "/promptTokensDetails/cachedTokens",
        "/prompt_tokens_details/cached_tokens",
        "/promptTokenDetails/cachedTokens",
        "/prompt_token_details/cached_tokens",
        "/numCachedTokens",
        "/num_cached_tokens",
    ]
    .iter()
    .find_map(|path| usage.pointer(path).filter(|v| !v.is_null()));
    let cached = raw.and_then(Value::as_f64).unwrap_or(0.0).max(0.0) as u64;
    prompt_tokens.min(cached)
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        other => (StopReason::Error, Some(format!("Provider stopped with: {other}"))),
    }
}

struct Run<'s, 'e> {
    output: AssistantMessage,
    sink: &'s mut EventSink<'e>,
    current: Option<usize>,
    /// Tool blocks by stream key (`index`, else id) with their accumulated argument text.
    tools: IndexMap<String, (usize, String)>,
}

impl Run<'_, '_> {
    fn emit(&mut self, event: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        let event = event(self.output.clone());
        (self.sink)(&event);
    }

    fn close_block(&mut self) {
        let Some(index) = self.current.take() else { return };
        match self.output.content[index].clone() {
            AssistantContent::Text(t) => self.emit(|partial| AssistantMessageEvent::TextEnd {
                content_index: index,
                content: t.text,
                partial,
            }),
            AssistantContent::Thinking(t) => self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                content_index: index,
                content: t.thinking,
                partial,
            }),
            AssistantContent::ToolCall(_) => {}
        }
    }

    fn push_text(&mut self, delta: &str, thinking: bool) {
        let open = self
            .current
            .map(|i| matches!(self.output.content[i], AssistantContent::Thinking(_)));
        if open != Some(thinking) {
            self.close_block();
            let index = self.output.content.len();
            self.current = Some(index);
            if thinking {
                self.output.content.push(AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                }));
                self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial,
                });
            } else {
                self.output.content.push(AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
                self.emit(|partial| AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial,
                });
            }
        }
        let index = self.output.content.len() - 1;
        let delta = delta.to_string();
        match &mut self.output.content[index] {
            AssistantContent::Thinking(t) => {
                t.thinking.push_str(&delta);
                self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial,
                });
            }
            AssistantContent::Text(t) => {
                t.text.push_str(&delta);
                self.emit(|partial| AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                    partial,
                });
            }
            AssistantContent::ToolCall(_) => {}
        }
    }

    fn handle(&mut self, chunk: &Value, model: &Model) {
        if self.output.response_id.is_none()
            && let Some(id) = chunk.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())
        {
            self.output.response_id = Some(id.to_string());
        }
        if let Some(usage) = chunk.get("usage").filter(|u| js_truthy(u)) {
            let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            let prompt = count("prompt_tokens");
            let cached = cached_prompt_tokens(usage, prompt);
            let u = &mut self.output.usage;
            u.input = prompt - cached;
            u.output = count("completion_tokens");
            u.cache_read = cached;
            u.cache_write = 0;
            u.total_tokens = match count("total_tokens") {
                0 => u.input + u.output + u.cache_read,
                total => total,
            };
            calculate_cost(model, &mut self.output.usage);
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return;
        };
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            self.output.raw_stop_reason = Some(reason.to_string());
            let (stop, error) = map_stop_reason(reason);
            self.output.stop_reason = stop;
            if error.is_some() {
                self.output.error_message = error;
            }
        }
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
        let items = match delta.get("content") {
            Some(Value::String(s)) => vec![Value::String(s.clone())],
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        for item in &items {
            match item {
                Value::String(text) => self.push_text(text, false),
                _ if item["type"] == "thinking" => {
                    let text: String = item
                        .get("thinking")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect();
                    if !text.is_empty() {
                        self.push_text(&text, true);
                    }
                }
                _ if item["type"] == "text" => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                    self.push_text(&text, false);
                }
                _ => {}
            }
        }
        for call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            self.close_block();
            let stream_index = call.get("index").and_then(Value::as_i64);
            let call_id = match call.get("id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() && id != "null" => id.to_string(),
                _ => derive_mistral_tool_call_id(&format!("toolcall:{}", stream_index.unwrap_or(0)), 0),
            };
            let key = match stream_index {
                Some(i) => format!("index:{i}"),
                None => format!("id:{call_id}"),
            };
            if !self.tools.contains_key(&key) {
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let index = self.output.content.len();
                self.output.content.push(AssistantContent::ToolCall(ToolCall {
                    id: call_id,
                    name: name.to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                    namespace: None,
                }));
                self.tools.insert(key.clone(), (index, String::new()));
                self.emit(|partial| AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial,
                });
            }
            let args_delta = match call.pointer("/function/arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) if js_truthy(other) => stringify(other),
                _ => "{}".to_string(),
            };
            let (index, partial_args) = self
                .tools
                .get_mut(&key)
                .map(|(i, p)| (*i, p))
                .unwrap_or_else(|| unreachable!());
            partial_args.push_str(&args_delta);
            let arguments = parse_streaming_json(partial_args);
            if let AssistantContent::ToolCall(block) = &mut self.output.content[index] {
                block.arguments = arguments;
            }
            self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                content_index: index,
                delta: args_delta,
                partial,
            });
        }
    }

    fn finish(&mut self) {
        self.close_block();
        let tools: Vec<(usize, String)> = self.tools.values().cloned().collect();
        for (index, partial_args) in tools {
            let AssistantContent::ToolCall(block) = &mut self.output.content[index] else {
                continue;
            };
            block.arguments = parse_streaming_json(&partial_args);
            let tool_call = block.clone();
            self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                content_index: index,
                tool_call,
                partial,
            });
        }
    }
}

fn uses_reasoning_effort(model: &Model) -> bool {
    model.id == "mistral-small-2603"
        || model.id == "mistral-small-latest"
        || model.id.starts_with("mistral-medium-")
        || model.id == "zai-glm-5-2"
}

/// `streamSimple`: `promptMode: "reasoning"` or a `reasoningEffort`, depending on the model family.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    if options.base.api_key.as_deref().is_none_or(str::is_empty) {
        return Err(format!("No API key for provider: {}", model.provider));
    }
    let reasoning = options
        .reasoning
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(|r| clamp_thinking_level(model, r))
        .filter(|r| *r != "off");
    let effort_model = uses_reasoning_effort(model);
    let mistral = MistralOptions {
        base: build_base_options(model, context, options),
        tool_choice: options.tool_choice.clone().map(Value::String),
        prompt_mode: reasoning.filter(|_| !effort_model).map(|_| "reasoning".to_string()),
        reasoning_effort: reasoning
            .filter(|_| effort_model)
            .map(|level| match model.thinking_level(level) {
                Mapped::Value(v) => v.to_string(),
                _ => "high".to_string(),
            }),
    };
    Ok(stream(model, context, &mistral, transport, sink).await)
}
