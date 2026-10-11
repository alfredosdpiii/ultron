//! pi-messages (`api/pi-messages.ts`): Pi's own message protocol sent directly to a backend (the Radius gateway, or any
//! `"api": "pi-messages"` custom provider). One POST of `{ model, context, options }` to `<baseUrl>/messages`; the
//! response is an SSE stream of serialized assistant-message events ending in `done` or `error`.

use crate::env::provider_env_value;
use crate::http::{AbortSignal, HttpRequest, HttpResponse, Read, Transport, cancelled, is_aborted, read_chunk};
use crate::json::{parse_json, parse_streaming_json};
use crate::jsutil::{Utf8Stream, js_trim};
use crate::options::{SimpleStreamOptions, StreamOptions, now_ms};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention,
    DiagnosticErrorInfo, EventSink, Model, StopReason, TextContent, ThinkingContent, ToolCall, TranscriptContext,
    Usage,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// `PiMessagesOptions`.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiMessagesOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub reasoning: Option<String>,
    /// `"auto" | "none" | "required" | { type: "function", function: { name } }`.
    pub tool_choice: Option<Value>,
    /// Ask the backend for debug metadata (`?debug=1`).
    pub debug: Option<bool>,
}

/// A failed request: the message, and for an HTTP failure the `PiMessagesResponseError` code and diagnostic details.
struct Failure {
    message: String,
    response: Option<(Option<String>, Map<String, Value>)>,
}

impl Failure {
    fn plain(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            response: None,
        }
    }
}

const MAX_DIAGNOSTIC_CHARS: usize = 8192;

fn truncate_diagnostic(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() > MAX_DIAGNOSTIC_CHARS {
        format!("{}…", String::from_utf16_lossy(&units[..MAX_DIAGNOSTIC_CHARS]))
    } else {
        value.to_string()
    }
}

fn status_text(status: u16) -> String {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or_default()
        .to_string()
}

/// `createPiMessagesResponseError`.
fn response_failure(model: &Model, url: &str, status: u16, body: &str) -> Failure {
    let error_body = parse_json(body)
        .ok()
        .filter(|parsed| parsed.get("error").is_some_and(Value::is_object));
    let error = error_body.as_ref().and_then(|b| b.get("error"));
    let message = error.and_then(|e| e.get("message")).and_then(Value::as_str);
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let status_text = status_text(status);
    let code_suffix = code
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|c| format!(" ({c})"))
        .unwrap_or_default();
    let text = format!("{status} {status_text}: {}{code_suffix}", message.unwrap_or(body));
    let mut details = Map::new();
    details.insert("version".into(), json!(1));
    details.insert("provider".into(), json!(model.provider));
    details.insert("model".into(), json!(model.id));
    details.insert("url".into(), json!(url));
    details.insert("status".into(), json!(status));
    details.insert("statusText".into(), json!(status_text));
    if let Some(error) = error {
        details.insert("error".into(), error.clone());
    }
    if error_body.is_none() {
        details.insert("body".into(), json!(truncate_diagnostic(body)));
    }
    details.insert("timestampMs".into(), json!(now_ms()));
    Failure {
        message: text,
        response: Some((code, details)),
    }
}

/// `createEventConverter`: wire events applied to one partial message.
struct Converter {
    partial: AssistantMessage,
    tool_json: HashMap<usize, String>,
}

fn index_of(event: &Value) -> usize {
    event.get("contentIndex").and_then(Value::as_u64).unwrap_or(0) as usize
}

fn str_of(event: &Value, key: &str) -> Option<String> {
    event.get(key).and_then(Value::as_str).map(str::to_string)
}

impl Converter {
    fn set(&mut self, index: usize, block: AssistantContent) {
        let content = &mut self.partial.content;
        if index < content.len() {
            content[index] = block;
        } else {
            // Upstream assigns into a sparse array; the protocol always appends in order.
            content.push(block);
        }
    }

    fn finish(&mut self, event: &Value) -> Result<(), String> {
        self.partial.stop_reason =
            serde_json::from_value(event.get("reason").cloned().unwrap_or(Value::Null)).map_err(|e| e.to_string())?;
        self.partial.usage = serde_json::from_value(event.get("usage").cloned().unwrap_or(Value::Null))
            .unwrap_or_else(|_| Usage::default());
        self.partial.response_id = str_of(event, "responseId");
        if let Some(level) = str_of(event, "providerThinkingLevel") {
            self.partial.provider_thinking_level = Some(level);
        }
        if let Some(rewrite) = event.get("rewrite").and_then(Value::as_object) {
            self.partial
                .diagnostics
                .get_or_insert_with(Vec::new)
                .push(AssistantMessageDiagnostic {
                    kind: "pi_messages_rewrite".into(),
                    timestamp: now_ms(),
                    error: None,
                    details: Some(rewrite.clone()),
                });
        }
        Ok(())
    }

    /// One wire event as a stream event; `Ok(None)` for nothing to emit.
    fn convert(&mut self, event: &Value) -> Result<Option<AssistantMessageEvent>, String> {
        let index = index_of(event);
        let partial = |c: &Converter| c.partial.clone();
        Ok(Some(
            match event.get("type").and_then(Value::as_str).unwrap_or_default() {
                "done" => {
                    self.finish(event)?;
                    AssistantMessageEvent::Done {
                        reason: self.partial.stop_reason,
                        message: partial(self),
                    }
                }
                "error" => {
                    self.finish(event)?;
                    self.partial.error_message = str_of(event, "errorMessage");
                    AssistantMessageEvent::Error {
                        reason: self.partial.stop_reason,
                        error: partial(self),
                    }
                }
                "start" => AssistantMessageEvent::Start { partial: partial(self) },
                "text_start" => {
                    self.set(
                        index,
                        AssistantContent::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }),
                    );
                    AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: partial(self),
                    }
                }
                "text_delta" => {
                    let delta = str_of(event, "delta").unwrap_or_default();
                    if let Some(AssistantContent::Text(block)) = self.partial.content.get_mut(index) {
                        block.text.push_str(&delta);
                    }
                    AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta,
                        partial: partial(self),
                    }
                }
                "text_end" => {
                    let content = str_of(event, "content").unwrap_or_default();
                    if let Some(AssistantContent::Text(block)) = self.partial.content.get_mut(index) {
                        block.text = content.clone();
                        block.text_signature = str_of(event, "contentSignature");
                    }
                    AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content,
                        partial: partial(self),
                    }
                }
                "thinking_start" => {
                    self.set(
                        index,
                        AssistantContent::Thinking(ThinkingContent {
                            thinking: String::new(),
                            thinking_signature: None,
                            redacted: None,
                        }),
                    );
                    AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: partial(self),
                    }
                }
                "thinking_delta" => {
                    let delta = str_of(event, "delta").unwrap_or_default();
                    if let Some(AssistantContent::Thinking(block)) = self.partial.content.get_mut(index) {
                        block.thinking.push_str(&delta);
                    }
                    AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta,
                        partial: partial(self),
                    }
                }
                "thinking_end" => {
                    let content = str_of(event, "content").unwrap_or_default();
                    if let Some(AssistantContent::Thinking(block)) = self.partial.content.get_mut(index) {
                        block.thinking = content.clone();
                        block.thinking_signature = str_of(event, "contentSignature");
                        block.redacted = event.get("redacted").and_then(Value::as_bool);
                    }
                    AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content,
                        partial: partial(self),
                    }
                }
                "toolcall_start" => {
                    self.set(
                        index,
                        AssistantContent::ToolCall(ToolCall {
                            id: str_of(event, "id").unwrap_or_default(),
                            name: str_of(event, "toolName").unwrap_or_default(),
                            arguments: json!({}),
                            thought_signature: None,
                            namespace: None,
                        }),
                    );
                    self.tool_json.insert(index, String::new());
                    AssistantMessageEvent::ToolCallStart {
                        content_index: index,
                        partial: partial(self),
                    }
                }
                "toolcall_delta" => {
                    let delta = str_of(event, "delta").unwrap_or_default();
                    let buffer = self.tool_json.entry(index).or_default();
                    buffer.push_str(&delta);
                    let arguments = parse_streaming_json(buffer);
                    if let Some(AssistantContent::ToolCall(call)) = self.partial.content.get_mut(index) {
                        call.arguments = arguments;
                    }
                    AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta,
                        partial: partial(self),
                    }
                }
                "toolcall_end" => {
                    let wire = event
                        .get("toolCall")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let Some(AssistantContent::ToolCall(call)) = self.partial.content.get_mut(index) else {
                        return Ok(None);
                    };
                    let mut merged = serde_json::to_value(&*call).map_err(|e| e.to_string())?;
                    for (key, value) in wire {
                        if key != "type" {
                            merged[key] = value;
                        }
                    }
                    *call = serde_json::from_value(merged).map_err(|e| e.to_string())?;
                    let tool_call = call.clone();
                    self.tool_json.remove(&index);
                    AssistantMessageEvent::ToolCallEnd {
                        content_index: index,
                        tool_call,
                        partial: partial(self),
                    }
                }
                _ => return Ok(None),
            },
        ))
    }
}

/// `parsePiMessagesEvent`: the first `data:` line of a frame, unless `[DONE]`.
fn parse_frame(raw: &str) -> Result<Option<Value>, String> {
    let Some(data) = raw
        .split('\n')
        .find(|line| line.starts_with("data:"))
        .map(|line| js_trim(&line[5..]))
    else {
        return Ok(None);
    };
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }
    parse_json(data).map(Some).map_err(|e| e.to_string())
}

async fn read_events(
    response: &mut HttpResponse,
    signal: Option<&AbortSignal>,
    mut on_event: impl FnMut(Value) -> Result<bool, String>,
) -> Result<bool, Failure> {
    let mut decoder = Utf8Stream::default();
    let mut buffer = String::new();
    loop {
        // The body errors once the request is aborted; frames already read are still delivered.
        if is_aborted(signal) {
            return Err(Failure::plain("This operation was aborted"));
        }
        let (text, done) = match read_chunk(response.body.as_mut(), signal).await {
            Read::Chunk(bytes) => (decoder.push(&bytes), false),
            Read::End => (decoder.finish(), true),
            Read::Aborted => return Err(Failure::plain("This operation was aborted")),
            Read::Failed(error) => return Err(Failure::plain(error)),
        };
        buffer.push_str(&text);
        buffer = buffer.replace("\r\n", "\n");
        while let Some(split) = buffer.find("\n\n") {
            let frame: String = buffer[..split].to_string();
            buffer.drain(..split + 2);
            if let Some(event) = parse_frame(&frame).map_err(Failure::plain)?
                && on_event(event).map_err(Failure::plain)?
            {
                return Ok(true);
            }
        }
        if done {
            break;
        }
    }
    if !js_trim(&buffer).is_empty()
        && let Some(event) = parse_frame(&buffer).map_err(Failure::plain)?
    {
        return on_event(event).map_err(Failure::plain);
    }
    Ok(false)
}

fn resolve_cache_retention(options: &StreamOptions) -> Option<CacheRetention> {
    options.cache_retention.or_else(|| {
        (provider_env_value("PI_CACHE_RETENTION", options.env.as_ref()).as_deref() == Some("long"))
            .then_some(CacheRetention::Long)
    })
}

/// The request URL: `<baseUrl>/messages`, with `?debug=1` when asked.
fn request_url(model: &Model, debug: bool) -> Result<String, String> {
    let mut url = reqwest::Url::parse(&format!("{}/messages", model.base_url.trim_end_matches('/')))
        .map_err(|_| format!("Invalid URL: {}/messages", model.base_url.trim_end_matches('/')))?;
    if debug {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "debug")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .append_pair("debug", "1");
    }
    Ok(url.to_string())
}

fn payload(model: &Model, context: &TranscriptContext, options: &PiMessagesOptions) -> Value {
    let base = &options.base;
    let mut request_options = Map::new();
    if let Some(temperature) = base.temperature {
        request_options.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = base.max_tokens {
        request_options.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(reasoning) = &options.reasoning {
        request_options.insert("reasoning".into(), json!(reasoning));
    }
    if let Some(retention) = resolve_cache_retention(base) {
        request_options.insert("cacheRetention".into(), json!(retention));
    }
    if let Some(session) = &base.session_id {
        request_options.insert("sessionId".into(), json!(session));
    }
    if let Some(choice) = &options.tool_choice {
        request_options.insert("toolChoice".into(), choice.clone());
    }
    json!({"model": model.id, "context": context, "options": request_options})
}

async fn drive(
    model: &Model,
    context: &TranscriptContext,
    options: &PiMessagesOptions,
    transport: &dyn Transport,
    converter: &mut Converter,
    sink: &mut EventSink<'_>,
) -> Result<(), Failure> {
    let base = &options.base;
    let signal = base.signal();
    let api_key = base
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Failure::plain(format!("No API key provided for provider \"{}\"", model.provider)))?;
    let url = request_url(model, options.debug == Some(true)).map_err(Failure::plain)?;
    let mut body = payload(model, context, options);
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&body, model)) {
        body = next;
    }
    let mut headers = crate::http::HeaderMap::new();
    headers.insert("authorization".into(), format!("Bearer {api_key}"));
    headers.insert("accept".into(), "text/event-stream".into());
    headers.insert("content-type".into(), "application/json".into());
    for (name, value) in base.headers.iter().flatten() {
        if let Some(value) = value {
            headers.insert(name.to_ascii_lowercase(), value.clone());
        }
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: url.clone(),
        headers,
        body: serde_json::to_string(&body).unwrap_or_default(),
    };
    if is_aborted(signal) {
        return Err(Failure::plain("This operation was aborted"));
    }
    let sent = tokio::select! {
        sent = transport.send(request) => sent,
        () = cancelled(signal) => return Err(Failure::plain("This operation was aborted")),
    };
    let mut response = sent.map_err(Failure::plain)?;
    if !(200..300).contains(&response.status) {
        let mut decoder = Utf8Stream::default();
        let mut text = String::new();
        while let Ok(Some(chunk)) = response.body.next_chunk().await {
            text.push_str(&decoder.push(&chunk));
        }
        text.push_str(&decoder.finish());
        return Err(response_failure(model, &url, response.status, &text));
    }
    let terminal = read_events(&mut response, signal, |wire| {
        let Some(event) = converter.convert(&wire)? else {
            return Ok(false);
        };
        sink(&event);
        Ok(matches!(
            event,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        ))
    })
    .await?;
    if terminal {
        Ok(())
    } else {
        Err(Failure::plain(format!(
            "{} stream ended without a terminal event",
            model.provider
        )))
    }
}

/// Stream one response. A terminal `done`/`error` from the backend ends the stream; every failure ends with an
/// `error` event carrying a fresh message (no partial content).
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &PiMessagesOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let mut converter = Converter {
        partial: AssistantMessage::pending(model, now_ms()),
        tool_json: HashMap::new(),
    };
    let mut last: Option<AssistantMessage> = None;
    let outcome = {
        let mut record = |event: &AssistantMessageEvent| {
            if let AssistantMessageEvent::Done { message, .. } | AssistantMessageEvent::Error { error: message, .. } =
                event
            {
                last = Some(message.clone());
            }
            sink(event);
        };
        drive(model, context, options, transport, &mut converter, &mut record).await
    };
    match outcome {
        Ok(()) => last.unwrap_or(converter.partial),
        Err(failure) => {
            let aborted = is_aborted(options.base.signal());
            let mut message = AssistantMessage::pending(model, now_ms());
            message.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            message.error_message = Some(failure.message.clone());
            if !aborted && let Some((code, details)) = failure.response {
                message.diagnostics = Some(vec![AssistantMessageDiagnostic {
                    kind: "pi_messages_response_failure".into(),
                    timestamp: now_ms(),
                    error: Some(DiagnosticErrorInfo {
                        name: Some("PiMessagesResponseError".into()),
                        message: failure.message,
                        stack: None,
                        code: code.map(Value::String),
                    }),
                    details: Some(details),
                }]);
            }
            let reason = message.stop_reason;
            sink(&AssistantMessageEvent::Error {
                reason,
                error: message.clone(),
            });
            message
        }
    }
}

/// `streamSimple`: the simple options as they are, plus `reasoning` and `toolChoice`.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let options = PiMessagesOptions {
        base: options.base.clone(),
        reasoning: options.reasoning.clone(),
        tool_choice: options.tool_choice.clone().map(Value::String),
        debug: None,
    };
    stream(model, context, &options, transport, sink).await
}
