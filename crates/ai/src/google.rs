//! `api/google-generative-ai.ts`: the Gemini API provider. Upstream drives the `@google/genai` SDK; this port builds the
//! SDK's request parameters, serializes them as the SDK does (`models/{id}:streamGenerateContent?alt=sse`), and parses
//! the SDK's SSE framing. Evidence: `fixtures/generate-google.mjs` records the TypeScript provider against canned
//! responses served through a stubbed `fetch`; `tests/providers_more.rs` replays them.

use crate::google_shared::{
    contents_to_wire, convert_messages, convert_tools, disabled_google_thinking_config, map_stop_reason,
    resolve_google_function_calling_mode, resolve_google_thinking_level, retain_thought_signature,
    supports_google_strict_tool_sampling, to_google_thinking_level, uses_google_thinking_level,
};
use crate::http::{
    Failure, HeaderBuilder, Read, RetryOptions, cancelled, format_provider_error, is_aborted, read_chunk,
};
use crate::json::{parse_json, stringify};
use crate::models::{calculate_cost, clamp_thinking_level};
use crate::options::{SimpleStreamOptions, StreamOptions, ThinkingBudgets, build_base_options, now_ms, pi_user_agent};
use crate::transcript::{collapse_system_messages, current_tools, initial_system_message, system_message_text};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, EventSink, Model, StopReason, TextContent,
    ThinkingContent, ToolCall, TranscriptContext, Usage,
};
use crate::{AbortSignal, HttpRequest, HttpResponse, Transport};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleThinking {
    pub enabled: bool,
    /// `-1` for dynamic, `0` to disable.
    pub budget_tokens: Option<i64>,
    /// `MINIMAL | LOW | MEDIUM | HIGH`.
    pub level: Option<String>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    /// `"auto" | "none" | "any"`.
    pub tool_choice: Option<String>,
    pub thinking: Option<GoogleThinking>,
}

/// Upstream's module-level counters for generated tool call ids, one per provider module.
static GEMINI_TOOL_CALLS: AtomicU64 = AtomicU64::new(0);
static VERTEX_TOOL_CALLS: AtomicU64 = AtomicU64::new(0);

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// The SDK's `GenerateContentParameters` (`{model, contents, config}`) for this request; `onPayload` sees this shape.
pub fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: &GoogleOptions,
    now: i64,
) -> Result<Value, String> {
    let contents = convert_messages(model, context, now);
    let tools = current_tools(&context.messages);
    let mut config = Map::new();
    if let Some(t) = options.base.temperature {
        config.insert("temperature".into(), json!(t));
    }
    if let Some(m) = options.base.max_tokens {
        config.insert("maxOutputTokens".into(), json!(m));
    }
    let strict = supports_google_strict_tool_sampling(&model.id);
    let mode = if tools.is_empty() {
        None
    } else {
        resolve_google_function_calling_mode(&tools, options.tool_choice.as_deref(), strict)?
    };
    let system = initial_system_message(&context.messages)
        .map(system_message_text)
        .unwrap_or_default();
    if !system.is_empty() {
        config.insert("systemInstruction".into(), system.into());
    }
    if let Some(declarations) = convert_tools(&tools, false, strict)? {
        config.insert("tools".into(), declarations);
    }
    if let Some(mode) = mode {
        config.insert(
            "toolConfig".into(),
            json!({ "functionCallingConfig": { "mode": mode } }),
        );
    }
    match &options.thinking {
        Some(thinking) if thinking.enabled && model.reasoning => {
            let mut thinking_config = Map::new();
            thinking_config.insert("includeThoughts".into(), true.into());
            if let Some(level) = &thinking.level {
                thinking_config.insert("thinkingLevel".into(), level.clone().into());
            } else if let Some(budget) = thinking.budget_tokens {
                thinking_config.insert("thinkingBudget".into(), budget.into());
            }
            config.insert("thinkingConfig".into(), Value::Object(thinking_config));
        }
        Some(thinking) if model.reasoning && !thinking.enabled => {
            config.insert("thinkingConfig".into(), disabled_google_thinking_config(model)?);
        }
        _ => {}
    }
    if is_aborted(options.base.signal()) {
        return Err("Request aborted".into());
    }
    Ok(json!({ "model": model.id, "contents": contents, "config": config }))
}

const GENERATION_CONFIG_KEYS: [&str; 14] = [
    "temperature",
    "topP",
    "topK",
    "candidateCount",
    "maxOutputTokens",
    "stopSequences",
    "responseLogprobs",
    "logprobs",
    "presencePenalty",
    "frequencyPenalty",
    "seed",
    "responseMimeType",
    "responseJsonSchema",
    "thinkingConfig",
];

/// The SDK's `generateContentParametersToMldev` (or `...ToVertex`): request body fields in the SDK's key order.
pub fn params_to_wire(params: &Value, vertex: bool) -> Value {
    let config = params
        .get("config")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut body = Map::new();
    if let Some(Value::Array(contents)) = params.get("contents") {
        body.insert("contents".into(), Value::Array(contents_to_wire(contents, vertex)));
    }
    if let Some(system) = config.get("systemInstruction") {
        let content = match system {
            Value::String(text) => json!({ "parts": [{ "text": text }], "role": "user" }),
            other => other.clone(),
        };
        body.insert("systemInstruction".into(), content);
    }
    for key in ["safetySettings", "tools", "toolConfig", "cachedContent"] {
        if let Some(value) = config.get(key).filter(|v| !v.is_null()) {
            body.insert(key.into(), value.clone());
        }
    }
    let generation: Map<String, Value> = GENERATION_CONFIG_KEYS
        .iter()
        .filter_map(|key| {
            config
                .get(*key)
                .filter(|v| !v.is_null())
                .map(|v| ((*key).to_string(), v.clone()))
        })
        .collect();
    body.insert("generationConfig".into(), Value::Object(generation));
    Value::Object(body)
}

/// The SDK's `tModel`.
fn model_path(id: &str, vertex: bool) -> String {
    if !vertex {
        return if id.starts_with("models/") || id.starts_with("tunedModels/") {
            id.to_string()
        } else {
            format!("models/{id}")
        };
    }
    if id.starts_with("publishers/") || id.starts_with("projects/") || id.starts_with("models/") {
        id.to_string()
    } else if let Some((publisher, name)) = id.split_once('/') {
        format!(
            "publishers/{publisher}/models/{}",
            name.split('/').next().unwrap_or_default()
        )
    } else {
        format!("publishers/google/models/{id}")
    }
}

/// Where and how a Google request goes: the versioned base URL, the credential, the Vertex resource path
/// (`projects/<p>/locations/<l>`, when the SDK prepends it), and which SDK transform applies.
pub(crate) struct Endpoint {
    pub base: String,
    pub auth: EndpointAuth,
    pub resource: Option<String>,
    pub vertex: bool,
}

/// An API key header, or Google application default credentials resolved when the request is sent.
pub(crate) enum EndpointAuth {
    ApiKey(String),
    Google(Option<crate::env::ProviderEnv>),
}

/// Stream one Gemini API response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &GoogleOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let endpoint = match options.base.api_key.as_ref().filter(|k| !k.is_empty()) {
        Some(key) => Ok(Endpoint {
            base: if model.base_url.is_empty() {
                DEFAULT_BASE_URL.into()
            } else {
                model.base_url.trim_end_matches('/').into()
            },
            auth: EndpointAuth::ApiKey(key.clone()),
            resource: None,
            vertex: false,
        }),
        None => Err(format!("No API key for provider: {}", model.provider)),
    };
    stream_endpoint(model, context, options, endpoint, "Google", transport, sink).await
}

/// The stream shared by the Gemini API and Vertex providers; `label` names the provider in the no-finish error.
pub(crate) async fn stream_endpoint(
    model: &Model,
    context: &TranscriptContext,
    options: &GoogleOptions,
    endpoint: Result<Endpoint, String>,
    label: &str,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let context = collapse_system_messages(context);
    let mut run = Run {
        output: AssistantMessage::pending(model, now_ms()),
        sink,
        current: None,
        tool_calls: if label == "Google Vertex" {
            &VERTEX_TOOL_CALLS
        } else {
            &GEMINI_TOOL_CALLS
        },
    };
    let signal = options.base.signal();
    match drive(&mut run, model, &context, options, endpoint, label, transport).await {
        Ok(()) => {
            let reason = run.output.stop_reason;
            run.emit(|message| AssistantMessageEvent::Done { reason, message });
        }
        Err(failure) => {
            run.output.stop_reason = if is_aborted(signal) {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            run.output.error_message = Some(format_provider_error(&failure, None));
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
    options: &GoogleOptions,
    endpoint: Result<Endpoint, String>,
    label: &str,
    transport: &dyn Transport,
) -> Result<(), Failure> {
    let base = &options.base;
    let signal = base.signal();
    let endpoint = endpoint.map_err(Failure::plain)?;
    let mut params = build_params(model, context, options, run.output.timestamp).map_err(Failure::plain)?;
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&params, model)) {
        params = next;
    }
    let mut headers = HeaderBuilder::default();
    headers.set("Content-Type", "application/json");
    if let EndpointAuth::ApiKey(key) = &endpoint.auth {
        headers.set("x-goog-api-key", key.clone());
    }
    headers.set("User-Agent", pi_user_agent());
    headers.extend_set(model.headers.iter().flatten());
    for (name, value) in base.headers.iter().flatten() {
        if let Some(value) = value {
            headers.set(name, value.clone());
        }
    }
    let mut headers = headers.build();
    if let EndpointAuth::Google(env) = &endpoint.auth {
        // `NodeAuth.addGoogleAuthHeaders`: a header the caller already set wins.
        let auth = tokio::select! {
            auth = crate::google_auth::request_headers(env.as_ref(), transport) => auth.map_err(Failure::plain)?,
            () = cancelled(signal) => return Err(Failure::plain("Request aborted")),
        };
        headers.entry("authorization".into()).or_insert(auth.authorization);
        if let Some(project) = auth.user_project {
            headers.entry("x-goog-user-project".into()).or_insert(project);
        }
    }
    let id = params
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&model.id)
        .to_string();
    let request = HttpRequest {
        method: "POST".into(),
        url: format!(
            "{}/{}{}:streamGenerateContent?alt=sse",
            endpoint.base,
            endpoint
                .resource
                .as_ref()
                .filter(|_| !id.starts_with("projects/"))
                .map_or_else(String::new, |r| format!("{r}/")),
            model_path(&id, endpoint.vertex)
        ),
        headers,
        body: serde_json::to_string(&params_to_wire(&params, endpoint.vertex)).unwrap_or_default(),
    };
    let mut response = send_with_google_retry(transport, &request, base.retry(), signal).await?;
    run.emit(|partial| AssistantMessageEvent::Start { partial });
    read_google_stream(&mut response, signal, |chunk| run.handle(chunk, model)).await?;
    run.close_block();
    if is_aborted(signal) {
        return Err(Failure::plain("Request was aborted"));
    }
    match run.output.stop_reason {
        StopReason::Pending => Err(Failure::plain(format!("{label} stream ended without a finish reason"))),
        StopReason::Aborted | StopReason::Error => Err(Failure::plain(match &run.output.raw_stop_reason {
            Some(raw) => format!("Provider stopped with: {raw}"),
            None => "An unknown error occurred".into(),
        })),
        _ => Ok(()),
    }
}

/// `throwErrorIfNotOK`: the SDK's `ApiError` carries the JSON body (or a synthesized one) as its message, a status, and
/// no headers.
async fn send_once(
    transport: &dyn Transport,
    request: &HttpRequest,
    signal: Option<&AbortSignal>,
) -> Result<HttpResponse, Failure> {
    let sent = tokio::select! {
        sent = transport.send(request.clone()) => sent,
        () = cancelled(signal) => return Err(Failure::plain("This operation was aborted")),
    };
    let mut response = sent.map_err(|_| Failure::plain("fetch failed"))?;
    if (200..300).contains(&response.status) {
        return Ok(response);
    }
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.body.next_chunk().await {
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let is_json = response
        .headers
        .get("content-type")
        .is_some_and(|t| t.contains("application/json"));
    let body = if is_json {
        parse_json(&text).map_err(|e| Failure::plain(e.to_string()))?
    } else {
        // ponytail: the Transport carries no reason phrase, so `statusText` is "" as for HTTP/2 responses.
        json!({ "error": { "message": text, "code": response.status, "status": "" } })
    };
    let status = response.status;
    Err(Failure {
        status: (400..600).contains(&status).then_some(status),
        api: (400..600).contains(&status),
        ..Failure::plain(stringify(&body))
    })
}

/// `retryGoogleRequest`: the shared provider retry policy, but the SDK error has no headers, so only the status
/// decides and the delay is always the exponential backoff.
async fn send_with_google_retry(
    transport: &dyn Transport,
    request: &HttpRequest,
    retry: RetryOptions,
    signal: Option<&AbortSignal>,
) -> Result<HttpResponse, Failure> {
    let max_retries = retry.max_retries.unwrap_or(0);
    let mut retry_index = 0;
    loop {
        let failure = match send_once(transport, request, signal).await {
            Ok(response) => return Ok(response),
            Err(failure) => failure,
        };
        if is_aborted(signal) {
            return Err(Failure::plain("Request aborted"));
        }
        let retryable = failure.status.is_some_and(|s| matches!(s, 408 | 409 | 429) || s >= 500);
        if retry_index >= max_retries || !failure.api || !retryable {
            return Err(failure);
        }
        // ponytail: upstream also applies up to 25% jitter; the delay is not observable beyond timing.
        let delay = (0.5 * 2f64.powi(retry_index as i32)).min(8.0);
        retry_index += 1;
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs_f64(delay)) => {}
            () = cancelled(signal) => return Err(Failure::plain("Request aborted")),
        }
    }
}

/// The SDK's `processStreamResponse`: events split on `\n\n`, `\r\r` or `\r\n\r\n`; `data:` events parse as JSON. A
/// network chunk that is itself a JSON error object throws an `ApiError`. An abort throws the fetch `AbortError`.
async fn read_google_stream(
    response: &mut HttpResponse,
    signal: Option<&AbortSignal>,
    mut handle: impl FnMut(&Value) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        // A body errored by the abort rejects the next read.
        if is_aborted(signal) {
            return Err(Failure::plain("This operation was aborted"));
        }
        let chunk = match read_chunk(response.body.as_mut(), signal).await {
            Read::Chunk(bytes) => bytes,
            Read::End => break,
            Read::Aborted => return Err(Failure::plain("This operation was aborted")),
            Read::Failed(error) => return Err(Failure::plain(error)),
        };
        if let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(&chunk)
            && let Some(error) = object.get("error")
        {
            let code = error.get("code").and_then(Value::as_f64).unwrap_or(0.0);
            if (400.0..600.0).contains(&code) {
                let status = error.get("status").map_or("undefined".to_string(), |s| match s {
                    Value::String(s) => s.clone(),
                    other => stringify(other),
                });
                return Err(Failure {
                    status: Some(code as u16),
                    api: true,
                    ..Failure::plain(format!(
                        "got status: {status}. {}",
                        stringify(&Value::Object(object.clone()))
                    ))
                });
            }
        }
        buffer.extend_from_slice(&chunk);
        while let Some((at, len)) = next_delimiter(&buffer) {
            let event = String::from_utf8_lossy(&buffer[..at]).into_owned();
            buffer.drain(..at + len);
            let event = event.trim();
            if let Some(data) = event.strip_prefix("data:") {
                let data = data.trim();
                let parsed = parse_json(data)
                    .map_err(|e| Failure::plain(format!("exception parsing stream chunk {data}. SyntaxError: {e}")))?;
                handle(&parsed)?;
            }
        }
    }
    if !String::from_utf8_lossy(&buffer).trim().is_empty() {
        return Err(Failure::plain("Incomplete JSON segment at the end"));
    }
    Ok(())
}

fn next_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    [&b"\n\n"[..], b"\r\r", b"\r\n\r\n"]
        .iter()
        .filter_map(|d| buffer.windows(d.len()).position(|w| w == *d).map(|at| (at, d.len())))
        .min_by_key(|(at, _)| *at)
}

struct Run<'s, 'e> {
    output: AssistantMessage,
    sink: &'s mut EventSink<'e>,
    /// Index of the open text or thinking block.
    current: Option<usize>,
    tool_calls: &'static AtomicU64,
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

    fn handle(&mut self, chunk: &Value, model: &Model) -> Result<(), Failure> {
        if self.output.response_id.is_none()
            && let Some(id) = chunk
                .get("responseId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            self.output.response_id = Some(id.to_string());
        }
        let candidate = chunk.pointer("/candidates/0");
        for part in candidate
            .and_then(|c| c.pointer("/content/parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let signature = part.get("thoughtSignature").and_then(Value::as_str);
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                let thinking = part.get("thought") == Some(&Value::Bool(true));
                let open_kind = self
                    .current
                    .map(|i| matches!(self.output.content[i], AssistantContent::Thinking(_)));
                if open_kind != Some(thinking) {
                    self.close_block();
                    let index = self.output.content.len();
                    if thinking {
                        self.output.content.push(AssistantContent::Thinking(ThinkingContent {
                            thinking: String::new(),
                            thinking_signature: None,
                            redacted: None,
                        }));
                        self.current = Some(index);
                        self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                            content_index: index,
                            partial,
                        });
                    } else {
                        self.output.content.push(AssistantContent::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }));
                        self.current = Some(index);
                        self.emit(|partial| AssistantMessageEvent::TextStart {
                            content_index: index,
                            partial,
                        });
                    }
                }
                let index = self.output.content.len() - 1;
                let delta = text.to_string();
                match &mut self.output.content[index] {
                    AssistantContent::Thinking(t) => {
                        t.thinking.push_str(text);
                        t.thinking_signature = retain_thought_signature(t.thinking_signature.take(), signature);
                        self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta,
                            partial,
                        });
                    }
                    AssistantContent::Text(t) => {
                        t.text.push_str(text);
                        t.text_signature = retain_thought_signature(t.text_signature.take(), signature);
                        self.emit(|partial| AssistantMessageEvent::TextDelta {
                            content_index: index,
                            delta,
                            partial,
                        });
                    }
                    AssistantContent::ToolCall(_) => {}
                }
            }
            if let Some(call) = part.get("functionCall").filter(|c| c.is_object()) {
                self.close_block();
                let name = call.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let provided = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
                let duplicate = provided.is_some_and(|id| {
                    self.output
                        .content
                        .iter()
                        .any(|b| matches!(b, AssistantContent::ToolCall(c) if c.id == id))
                });
                let id = match provided {
                    Some(id) if !duplicate => id.to_string(),
                    _ => format!(
                        "{name}_{}_{}",
                        now_ms(),
                        self.tool_calls.fetch_add(1, Ordering::Relaxed) + 1
                    ),
                };
                let arguments = call
                    .get("args")
                    .filter(|a| !a.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let tool_call = ToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature: signature.filter(|s| !s.is_empty()).map(str::to_string),
                    namespace: None,
                };
                let index = self.output.content.len();
                self.output.content.push(AssistantContent::ToolCall(tool_call.clone()));
                self.emit(|partial| AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial,
                });
                let delta = stringify(&tool_call.arguments);
                self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta,
                    partial,
                });
                self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call,
                    partial,
                });
            }
        }
        if let Some(reason) = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            self.output.raw_stop_reason = Some(reason.to_string());
            self.output.stop_reason = map_stop_reason(reason).map_err(Failure::plain)?;
            let has_tool = self
                .output
                .content
                .iter()
                .any(|b| matches!(b, AssistantContent::ToolCall(_)));
            if has_tool && self.output.stop_reason == StopReason::Stop {
                self.output.stop_reason = StopReason::ToolUse;
            }
        }
        if let Some(meta) = chunk.get("usageMetadata").filter(|m| m.is_object()) {
            let count = |key: &str| meta.get(key).and_then(Value::as_u64).unwrap_or(0);
            let thoughts = count("thoughtsTokenCount");
            // ponytail: upstream can go negative if cached > prompt; u64 usage saturates at 0.
            self.output.usage = Usage {
                input: count("promptTokenCount").saturating_sub(count("cachedContentTokenCount")),
                output: count("candidatesTokenCount") + thoughts,
                cache_read: count("cachedContentTokenCount"),
                cache_write: 0,
                cache_write_1h: None,
                reasoning: Some(thoughts),
                total_tokens: count("totalTokenCount"),
                cost: Default::default(),
            };
            calculate_cost(model, &mut self.output.usage);
        }
        Ok(())
    }
}

/// `streamSimple`: reasoning clamped to the model, sent as a Gemini 3 level or a token budget.
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
    let google = simple_options(model, context, options, true)?;
    Ok(stream(model, context, &google, transport, sink).await)
}

/// The `streamSimple` mapping shared with Vertex; `lite_budgets` selects the Gemini API's 2.5-flash-lite table.
pub(crate) fn simple_options(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    lite_budgets: bool,
) -> Result<GoogleOptions, String> {
    let disabled = Some(GoogleThinking {
        enabled: false,
        ..Default::default()
    });
    let level = options
        .reasoning
        .as_deref()
        .map(|r| clamp_thinking_level(model, r))
        .filter(|l| *l != "off");
    let thinking = match level {
        None => disabled,
        Some(level) => {
            let resolved = resolve_google_thinking_level(model, level)?;
            Some(if uses_google_thinking_level(&model.id) {
                GoogleThinking {
                    enabled: true,
                    budget_tokens: None,
                    level: Some(to_google_thinking_level(resolved).into()),
                }
            } else {
                GoogleThinking {
                    enabled: true,
                    budget_tokens: Some(google_budget(
                        model,
                        resolved,
                        options.thinking_budgets.as_ref(),
                        lite_budgets,
                    )),
                    level: None,
                }
            })
        }
    };
    Ok(GoogleOptions {
        base: build_base_options(model, context, options),
        tool_choice: options.tool_choice.clone(),
        thinking,
    })
}

/// `getGoogleBudget`: a custom budget, else the per-family default, else `-1` (dynamic). Vertex has no 2.5-flash-lite
/// row (`lite_budgets: false`), so those models take the 2.5-flash budgets.
pub fn google_budget(model: &Model, level: &str, custom: Option<&ThinkingBudgets>, lite_budgets: bool) -> i64 {
    let custom = custom.and_then(|b| match level {
        "minimal" => b.minimal,
        "low" => b.low,
        "medium" => b.medium,
        _ => b.high,
    });
    if let Some(budget) = custom {
        return budget as i64;
    }
    let table: [i64; 4] = if model.id.contains("2.5-pro") {
        [128, 2048, 8192, 32768]
    } else if lite_budgets && model.id.contains("2.5-flash-lite") {
        [512, 2048, 8192, 24576]
    } else if model.id.contains("2.5-flash") {
        [128, 2048, 8192, 24576]
    } else {
        return -1;
    };
    match level {
        "minimal" => table[0],
        "low" => table[1],
        "medium" => table[2],
        _ => table[3],
    }
}
