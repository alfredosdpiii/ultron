//! OpenAI Codex (ChatGPT backend) Responses provider (`api/openai-codex-responses.ts`): the request body, headers and
//! account id from the ChatGPT JWT, the WebSocket transport ([`websocket`]: `auto`/`websocket`/`websocket-cached`,
//! with its session socket cache, continuation deltas, retries and the fall back to SSE), the SSE transport with its
//! own retry policy and friendly usage-limit errors, the Codex event mapping (`response.done` and friends), and
//! Codex service-tier pricing. The SSE body is sent uncompressed, as upstream does where `node:zlib` has no zstd.

mod websocket;

pub use websocket::{
    OpenAICodexWebSocketDebugStats, advance_openai_codex_websocket_clock, close_openai_codex_websocket_sessions,
    get_openai_codex_websocket_debug_stats, reset_openai_codex_websocket_debug_stats,
};

use crate::constrained::grammar_tool_input_properties;
use crate::http::{
    AbortSignal, Body, HeaderBuilder, HttpRequest, HttpResponse, Read, Transport, cancelled, is_aborted, read_chunk,
};
use crate::json::{parse_json, stringify};
use crate::jsutil::{Utf8Stream, atob, js_trim, latin1};
use crate::models::clamp_thinking_level;
use crate::openai_prompt_cache::clamp_openai_prompt_cache_key;
use crate::openai_responses::{
    ConvertOptions, OPENAI_TOOL_CALL_PROVIDERS, ResponsesRun, TierPricing, ToolOptions, convert_responses_messages,
    convert_responses_tools,
};
use crate::options::{SimpleStreamOptions, StreamOptions, build_base_options, now_ms, pi_user_agent};
use crate::transcript::{
    declared_tools, initial_system_message, resolve_transcript, resolve_transcript_tools, system_message_text,
};
use crate::types::{
    AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention, DiagnosticErrorInfo,
    EventSink, Mapped, Model, StopReason, TranscriptContext,
};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::time::Duration;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const BASE_DELAY_MS: f64 = 1000.0;
const DEFAULT_MAX_RETRY_DELAY_MS: f64 = 60_000.0;
const CODEX_RESPONSE_STATUSES: &[&str] = &[
    "completed",
    "incomplete",
    "failed",
    "cancelled",
    "queued",
    "in_progress",
];

/// `OpenAICodexResponsesOptions`.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICodexResponsesOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    /// `"none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"`.
    pub reasoning_effort: Option<String>,
    /// `"auto" | "concise" | "detailed" | "off" | "on"`; `null` means `auto`.
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub text_verbosity: Option<String>,
    pub tool_choice: Option<String>,
}

// ---------------------------------------------------------------------------------------------- request building

/// `extractAccountId`: `chatgpt_account_id` from the JWT's `https://api.openai.com/auth` claim.
pub fn extract_account_id(token: &str) -> Result<String, String> {
    let fail = || "Failed to extract accountId from token".to_string();
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(fail());
    }
    let payload = parse_json(&latin1(&atob(parts[1]).ok_or_else(fail)?)).map_err(|_| fail())?;
    match payload.get(JWT_CLAIM_PATH).and_then(|c| c.get("chatgpt_account_id")) {
        Some(Value::String(id)) if !id.is_empty() => Ok(id.clone()),
        Some(value) if !matches!(value, Value::Null | Value::Bool(false)) && value != &json!(0) => Ok(js_string(value)),
        _ => Err(fail()),
    }
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => stringify(other),
    }
}

/// `resolveCodexUrl`.
pub fn resolve_codex_url(base_url: &str) -> String {
    let raw = if js_trim(base_url).is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICodexResponsesOptions,
    cache_session_id: Option<&str>,
    grammar: &IndexMap<String, String>,
    now: i64,
) -> Result<Map<String, Value>, String> {
    let flag = |key: &str, default: bool| model.compat_bool(key).unwrap_or(default);
    let supports_additional_tools = flag("supportsAdditionalTools", false);
    let supports_tool_search = flag("supportsToolSearch", false);
    let tool_options = ToolOptions {
        null_strict: true,
        supports_strict_mode: flag("supportsStrictMode", true),
        supports_openai_grammar_tools: flag("supportsOpenAIGrammarTools", false),
        tool_search_result: false,
    };
    let tools = resolve_transcript_tools(&context.messages, supports_additional_tools || supports_tool_search);
    let input = convert_responses_messages(
        model,
        context,
        &ConvertOptions {
            tool_call_providers: OPENAI_TOOL_CALL_PROVIDERS,
            include_system_prompt: false,
            grammar,
            supports_mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            supports_additional_tools,
            supports_tool_search,
            tool_options,
        },
        now,
    )?;
    let instructions = initial_system_message(&context.messages)
        .map(system_message_text)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "You are a helpful assistant.".into());
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    body.insert("instructions".into(), json!(instructions));
    body.insert("input".into(), Value::Array(input));
    body.insert(
        "text".into(),
        json!({"verbosity": options.text_verbosity.as_deref().filter(|v| !v.is_empty()).unwrap_or("low")}),
    );
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if let Some(session) = cache_session_id {
        body.insert("prompt_cache_key".into(), json!(session));
    }
    body.insert(
        "tool_choice".into(),
        json!(options.tool_choice.as_deref().unwrap_or("auto")),
    );
    body.insert("parallel_tool_calls".into(), json!(true));
    if let Some(temperature) = options.base.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(tier) = &options.service_tier {
        body.insert("service_tier".into(), json!(tier));
    }
    if !tools.request_tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(convert_responses_tools(&tools.request_tools, tool_options)?),
        );
    }
    let summary = options.reasoning_summary.as_deref().unwrap_or("auto");
    if let Some(effort) = &options.reasoning_effort {
        let effort: Option<&str> = if effort == "none" {
            match model.thinking_level("off") {
                Mapped::Missing => Some("none"),
                Mapped::Unsupported => None,
                Mapped::Value(value) => Some(value),
            }
        } else {
            Some(model.mapped_or(effort))
        };
        if let Some(effort) = effort {
            body.insert("reasoning".into(), json!({"effort": effort, "summary": summary}));
        }
    } else if model.reasoning && model.thinking_level("off") != Mapped::Unsupported {
        let off = match model.thinking_level("off") {
            Mapped::Value(value) => value,
            _ => "none",
        };
        body.insert("reasoning".into(), json!({"effort": off}));
    }
    Ok(body)
}

/// `buildBaseCodexHeaders`.
fn base_headers(model: &Model, options: &StreamOptions, account_id: &str, token: &str) -> HeaderBuilder {
    let mut headers = HeaderBuilder::default();
    headers.extend_set(model.headers.iter().flatten());
    headers.extend(options.headers.iter().flatten());
    headers
        .set("Authorization", format!("Bearer {token}"))
        .set("chatgpt-account-id", account_id)
        .set("originator", "pi")
        .set("User-Agent", pi_user_agent());
    headers
}

/// `buildWebSocketHeaders`: the base headers with the WebSocket beta, and `request_id` as the session and request id.
fn websocket_headers(
    model: &Model,
    options: &StreamOptions,
    account_id: &str,
    token: &str,
    request_id: &str,
) -> HeaderBuilder {
    let mut headers = base_headers(model, options, account_id, token);
    for name in ["accept", "content-type", "openai-beta"] {
        headers.remove(name);
    }
    headers
        .set("OpenAI-Beta", websocket::OPENAI_BETA_RESPONSES_WEBSOCKETS)
        .set("x-client-request-id", request_id)
        .set("session-id", request_id);
    headers
}

fn sse_headers(
    model: &Model,
    options: &StreamOptions,
    account_id: &str,
    token: &str,
    session_id: Option<&str>,
) -> HeaderBuilder {
    let mut headers = base_headers(model, options, account_id, token);
    headers
        .set("OpenAI-Beta", "responses=experimental")
        .set("accept", "text/event-stream")
        .set("content-type", "application/json");
    if let Some(session_id) = session_id {
        headers
            .set("session-id", session_id)
            .set("x-client-request-id", session_id);
    }
    headers
}

fn normalize_timeout_ms(value: Option<f64>) -> Result<Option<u64>, String> {
    match value {
        None => Ok(None),
        Some(v) if !v.is_finite() || v < 0.0 => Err(format!("Invalid timeoutMs: {}", js_number(v))),
        Some(v) => Ok(Some(v.floor() as u64)),
    }
}

fn js_number(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        }
    } else if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

// --------------------------------------------------------------------------------------------------- errors / retry

/// Why a Codex request failed: the kind decides retries and the WebSocket fallback.
#[derive(Debug)]
enum CodexError {
    Plain(String),
    /// `RetryDelayExceededError`: never retried.
    RetryDelay(String),
    /// `CodexApiError`: an `error` or `response.failed` event, with its code.
    Api {
        message: String,
        code: Option<String>,
    },
    /// `CodexProtocolError`: an event that is not JSON.
    Protocol(String),
    /// `WebSocketCloseError`: the socket closed before the response completed.
    Closed {
        message: String,
        code: u16,
    },
}

impl CodexError {
    fn message(&self) -> &str {
        match self {
            CodexError::Plain(m) | CodexError::RetryDelay(m) | CodexError::Protocol(m) => m,
            CodexError::Api { message, .. } | CodexError::Closed { message, .. } => message,
        }
    }

    /// `isCodexNonTransportError`.
    fn is_non_transport(&self) -> bool {
        matches!(self, CodexError::Api { .. } | CodexError::Protocol(_))
    }

    fn api_code(&self) -> Option<&str> {
        match self {
            CodexError::Api { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// `extractDiagnosticError`.
    fn diagnostic(&self) -> DiagnosticErrorInfo {
        let (name, code) = match self {
            CodexError::Plain(_) | CodexError::RetryDelay(_) => ("Error", None),
            CodexError::Api { code, .. } => ("CodexApiError", code.as_ref().map(|c| json!(c))),
            CodexError::Protocol(_) => ("CodexProtocolError", None),
            CodexError::Closed { code, .. } => ("WebSocketCloseError", Some(json!(code))),
        };
        DiagnosticErrorInfo {
            name: Some(name.into()),
            message: self.message().to_string(),
            stack: None,
            code,
        }
    }
}

fn aborted_error() -> CodexError {
    CodexError::Plain("Request was aborted".into())
}

fn is_terminal_rate_limit_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "gousagelimiterror",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_retryable_error(status: u16, text: &str) -> bool {
    if status == 429 && is_terminal_rate_limit_error(text) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    crate::jsutil::loose_match(text, "rate", "limit")
        || text.to_lowercase().contains("overloaded")
        || crate::jsutil::loose_match(text, "service", "unavailable")
        || crate::jsutil::loose_match(text, "upstream", "connect")
        || crate::jsutil::loose_match(text, "connection", "refused")
}

/// `Number(text)` for a header value: blank is 0, otherwise the whole trimmed text must be numeric.
fn js_to_number(text: &str) -> Option<f64> {
    let trimmed = js_trim(text);
    if trimmed.is_empty() {
        return Some(0.0);
    }
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok().map(|v| v as f64);
    }
    match trimmed {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if trimmed.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

/// `getRetryAfterDelayMs`: `retry-after-ms`, else `retry-after` in seconds, else as an HTTP date (the time until
/// then, at least 0).
fn retry_after_delay_ms(headers: &crate::http::HeaderMap) -> Option<f64> {
    if let Some(value) = headers.get("retry-after-ms")
        && let Some(millis) = js_to_number(value).filter(|m| m.is_finite())
    {
        return Some(millis.max(0.0));
    }
    let retry_after = headers.get("retry-after").filter(|v| !v.is_empty())?;
    if let Some(seconds) = js_to_number(retry_after).filter(|s| s.is_finite()) {
        return Some((seconds * 1000.0).max(0.0));
    }
    crate::http::until_http_date_ms(retry_after).map(|ms| ms.max(0.0))
}

fn validate_retry_delay_ms(delay_ms: f64, max: Option<f64>) -> Result<f64, CodexError> {
    let max = max.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max > 0.0 && delay_ms > max {
        return Err(CodexError::RetryDelay(format!(
            "Server requested {}s retry delay (max: {}s)",
            (delay_ms / 1000.0).ceil(),
            (max / 1000.0).ceil()
        )));
    }
    Ok(delay_ms)
}

async fn sleep(ms: f64, signal: Option<&AbortSignal>) -> Result<(), CodexError> {
    if is_aborted(signal) {
        return Err(aborted_error());
    }
    tokio::select! {
        () = tokio::time::sleep(Duration::from_secs_f64(ms.max(0.0) / 1000.0)) => Ok(()),
        () = cancelled(signal) => Err(aborted_error()),
    }
}

fn status_text(status: u16) -> String {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or_default()
        .to_string()
}

fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `parseErrorResponse`: the error message, and a friendly one for ChatGPT usage limits.
pub fn parse_error_response(status: u16, raw: &str, now: i64) -> (String, Option<String>) {
    let mut message = if !raw.is_empty() {
        raw.to_string()
    } else {
        let text = status_text(status);
        if text.is_empty() { "Request failed".into() } else { text }
    };
    let mut friendly = None;
    if let Ok(parsed) = parse_json(raw)
        && let Some(err) = parsed.get("error").filter(|e| js_truthy(Some(e)))
    {
        let code = [err.get("code"), err.get("type")]
            .into_iter()
            .find(|v| js_truthy(*v))
            .flatten()
            .map(js_string)
            .unwrap_or_default();
        let lower = code.to_lowercase();
        if lower.contains("usage_limit_reached")
            || lower.contains("usage_not_included")
            || lower.contains("rate_limit_exceeded")
            || status == 429
        {
            let plan = match err.get("plan_type").filter(|p| js_truthy(Some(p))) {
                Some(plan) => format!(" ({} plan)", js_string(plan).to_lowercase()),
                None => String::new(),
            };
            let when = match err
                .get("resets_at")
                .filter(|r| js_truthy(Some(r)))
                .and_then(Value::as_f64)
            {
                Some(resets_at) => {
                    let minutes = ((resets_at * 1000.0 - now as f64) / 60000.0 + 0.5).floor().max(0.0);
                    format!(" Try again in ~{} min.", js_number(minutes))
                }
                None => String::new(),
            };
            friendly = Some(js_trim(&format!("You have hit your ChatGPT usage limit{plan}.{when}")).to_string());
        }
        if let Some(text) = err.get("message").filter(|m| js_truthy(Some(m))) {
            message = js_string(text);
        } else if let Some(friendly) = &friendly {
            message = friendly.clone();
        }
    }
    (message, friendly)
}

// ------------------------------------------------------------------------------------------------- SSE processing

/// `parseSSE`: frames end at a blank line (`\n\n`); `data:` lines are trimmed and joined; `[DONE]` is skipped.
#[derive(Default)]
struct CodexSse {
    decoder: Utf8Stream,
    buffer: String,
}

impl CodexSse {
    fn frames(&mut self, text: &str, done: bool) -> Result<Vec<Value>, CodexError> {
        self.buffer.push_str(text);
        if done && !js_trim(&self.buffer).is_empty() {
            self.buffer.push_str("\n\n");
        }
        let mut out = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let chunk: String = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            let lines: Vec<&str> = chunk
                .split('\n')
                .filter(|l| l.starts_with("data:"))
                .map(|l| js_trim(&l[5..]))
                .collect();
            if lines.is_empty() {
                continue;
            }
            let data = lines.join("\n");
            let data = js_trim(&data);
            if !data.is_empty() && data != "[DONE]" {
                let value = parse_json(data).map_err(|e| {
                    let message = crate::jsutil::json_parse_error(data).unwrap_or_else(|| e.to_string());
                    CodexError::Protocol(format!("Invalid Codex SSE JSON: {message}"))
                })?;
                out.push(value);
            }
        }
        Ok(out)
    }
}

/// `mapCodexEvents` for one event: `Ok(None)` skips it, a terminal event is returned as `response.completed`.
fn map_codex_event(event: Value, output: &mut AssistantMessage) -> Result<Option<(Value, bool)>, CodexError> {
    let Some(kind) = event.get("type").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let field = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    match kind {
        "error" => {
            let nested = event.get("error").filter(|e| e.is_object());
            let code = field(&event, "code").or_else(|| nested.and_then(|n| field(n, "code")));
            let message = field(&event, "message").or_else(|| nested.and_then(|n| field(n, "message")));
            let text = message
                .filter(|m| !m.is_empty())
                .or(code.clone().filter(|c| !c.is_empty()))
                .unwrap_or_else(|| stringify(&event));
            Err(CodexError::Api {
                message: format!("Codex error: {text}"),
                code,
            })
        }
        "response.failed" => {
            let message = event
                .pointer("/response/error/message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .unwrap_or("Codex response failed");
            Err(CodexError::Api {
                message: message.to_string(),
                code: event
                    .pointer("/response/error/code")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        "response.done" | "response.completed" | "response.incomplete" => {
            let mut event = event;
            if let Some(end_turn) = event.pointer("/response/end_turn").and_then(Value::as_bool) {
                output.end_turn = Some(end_turn);
            }
            if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) {
                let status = response
                    .get("status")
                    .and_then(Value::as_str)
                    .filter(|s| CODEX_RESPONSE_STATUSES.contains(s))
                    .map(str::to_string);
                match status {
                    Some(status) => response.insert("status".into(), json!(status)),
                    None => response.shift_remove("status"),
                };
            }
            event["type"] = json!("response.completed");
            Ok(Some((event, true)))
        }
        _ => Ok(Some((event, false))),
    }
}

async fn process_stream(
    run: &mut ResponsesRun<'_, '_>,
    response: &mut HttpResponse,
    model: &Model,
    grammar: &IndexMap<String, String>,
    service_tier: Option<&str>,
    signal: Option<&AbortSignal>,
) -> Result<(), CodexError> {
    let mut sse = CodexSse::default();
    let body: &mut dyn Body = response.body.as_mut();
    'read: loop {
        if is_aborted(signal) {
            return Err(aborted_error());
        }
        let (text, done) = match read_chunk(body, signal).await {
            Read::Chunk(bytes) => (sse.decoder.push(&bytes), false),
            Read::End => (sse.decoder.finish(), true),
            Read::Aborted => return Err(aborted_error()),
            Read::Failed(error) => return Err(CodexError::Plain(error)),
        };
        if is_aborted(signal) {
            return Err(aborted_error());
        }
        for event in sse.frames(&text, done)? {
            let Some((event, terminal)) = map_codex_event(event, &mut run.output)? else {
                continue;
            };
            run.handle(&event, model, grammar, TierPricing::Codex(service_tier))
                .map_err(CodexError::Plain)?;
            if terminal {
                break 'read;
            }
        }
        if done {
            break;
        }
    }
    run.end().map_err(CodexError::Plain)
}

// ----------------------------------------------------------------------------------------------------- the request

/// Stream one Codex response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICodexResponsesOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let context = resolve_transcript(
        context,
        model.compat_bool("supportsMidConvoSystemMessages").unwrap_or(false),
    );
    let now = now_ms();
    let mut output = AssistantMessage::pending(model, now);
    output.api = "openai-codex-responses".into();
    let mut run = ResponsesRun::new(output, sink);
    let signal = options.base.signal();
    match drive(&mut run, model, &context, options, transport, now).await {
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
            run.output.error_message = Some(error.message().to_string());
            let reason = run.output.stop_reason;
            run.emit(|error| AssistantMessageEvent::Error { reason, error });
        }
    }
    run.output
}

async fn drive(
    run: &mut ResponsesRun<'_, '_>,
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICodexResponsesOptions,
    transport: &dyn Transport,
    now: i64,
) -> Result<(), CodexError> {
    let base = &options.base;
    let signal = base.signal();
    let api_key = base
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| CodexError::Plain(format!("No API key for provider: {}", model.provider)))?;
    let account_id = extract_account_id(&api_key).map_err(CodexError::Plain)?;
    let grammar = grammar_tool_input_properties(
        &declared_tools(&context.messages),
        model.compat_bool("supportsOpenAIGrammarTools").unwrap_or(false),
    )
    .map_err(CodexError::Plain)?;
    let cache_session_id = if base.cache_retention == Some(CacheRetention::None) {
        None
    } else {
        base.session_id.as_deref()
    };
    let codex_session_id = cache_session_id.map(clamp_openai_prompt_cache_key);
    let mut body = Value::Object(
        build_request_body(model, context, options, codex_session_id.as_deref(), &grammar, now)
            .map_err(CodexError::Plain)?,
    );
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&body, model)) {
        body = next;
    }
    let websocket_request_id = codex_session_id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(websocket::uuid_v7);
    let headers = sse_headers(model, base, &account_id, &api_key, codex_session_id.as_deref()).build();
    let ws_headers = websocket_headers(model, base, &account_id, &api_key, &websocket_request_id).build();
    let body_json = serde_json::to_string(&body).unwrap_or_default();
    let http_timeout_ms = normalize_timeout_ms(base.timeout_ms.map(|t| t as f64)).map_err(CodexError::Plain)?;
    let connect_timeout_ms = normalize_timeout_ms(base.websocket_connect_timeout_ms).map_err(CodexError::Plain)?;
    let configured = base.transport.as_deref().filter(|t| !t.is_empty()).unwrap_or("auto");
    let mut start_emitted = false;
    let websocket_disabled = configured != "sse" && websocket::is_websocket_sse_fallback_active(cache_session_id);
    if websocket_disabled {
        websocket::record_websocket_sse_fallback(cache_session_id);
    }
    if configured != "sse" && !websocket_disabled {
        let url = websocket::resolve_codex_websocket_url(&model.base_url);
        let request = websocket::Request {
            url: &url,
            body: &body,
            headers: &ws_headers,
            model,
            grammar: &grammar,
            service_tier: options.service_tier.as_deref(),
            idle_timeout_ms: http_timeout_ms,
            connect_timeout_ms,
            cache_session_id,
            account_id: &account_id,
            // The continuation is used only where the caller asked for `auto` or `websocket-cached` (upstream reads
            // `options.transport`, not its default).
            transport_kind: base.transport.as_deref().unwrap_or_default(),
            signal,
        };
        let mut retried_connection_limit = false;
        let mut retried_missing_continuation = false;
        loop {
            let mut started = false;
            let attempt =
                websocket::process_websocket_stream(run, transport, &request, &mut started, &mut start_emitted).await;
            let error = match attempt {
                Ok(()) => {
                    if is_aborted(signal) {
                        return Err(aborted_error());
                    }
                    return assert_successful(&run.output);
                }
                Err(error) => error,
            };
            let aborted = is_aborted(signal);
            let connection_limit_before_start =
                !started && error.api_code() == Some(websocket::WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE);
            let previous_response_not_found = error.api_code() == Some(websocket::PREVIOUS_RESPONSE_NOT_FOUND_CODE);
            if !aborted && previous_response_not_found && !retried_missing_continuation {
                retried_missing_continuation = true;
                continue;
            }
            if !aborted && connection_limit_before_start && !retried_connection_limit {
                retried_connection_limit = true;
                continue;
            }
            if aborted || (error.is_non_transport() && !connection_limit_before_start) {
                return Err(error);
            }
            let mut details = Map::new();
            details.insert("configuredTransport".into(), json!(configured));
            if !started {
                details.insert("fallbackTransport".into(), json!("sse"));
            }
            details.insert("eventsEmitted".into(), json!(started));
            details.insert(
                "phase".into(),
                json!(if started {
                    "after_message_stream_start"
                } else {
                    "before_message_stream_start"
                }),
            );
            details.insert("requestBytes".into(), json!(body_json.len()));
            run.output
                .diagnostics
                .get_or_insert_with(Vec::new)
                .push(AssistantMessageDiagnostic {
                    kind: "provider_transport_failure".into(),
                    timestamp: now_ms(),
                    error: Some(error.diagnostic()),
                    details: Some(details),
                });
            websocket::record_websocket_failure(cache_session_id, error.message());
            if started {
                return Err(error);
            }
            websocket::record_websocket_sse_fallback(cache_session_id);
            break;
        }
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: resolve_codex_url(&model.base_url),
        headers,
        body: body_json,
    };
    let mut response = send_with_codex_retry(transport, &request, base, http_timeout_ms, signal).await?;
    if !start_emitted {
        run.emit(|partial| AssistantMessageEvent::Start { partial });
    }
    process_stream(
        run,
        &mut response,
        model,
        &grammar,
        options.service_tier.as_deref(),
        signal,
    )
    .await?;
    if is_aborted(signal) {
        return Err(aborted_error());
    }
    assert_successful(&run.output)
}

/// `assertSuccessfulOutput`.
fn assert_successful(output: &AssistantMessage) -> Result<(), CodexError> {
    match output.stop_reason {
        StopReason::Pending => Err(CodexError::Plain("Codex stream ended without a stop reason".into())),
        StopReason::Error | StopReason::Aborted => Err(CodexError::Plain(
            output
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "An unknown error occurred".into()),
        )),
        _ => Ok(()),
    }
}

async fn read_text(response: &mut HttpResponse) -> String {
    let mut decoder = Utf8Stream::default();
    let mut text = String::new();
    while let Ok(Some(chunk)) = response.body.next_chunk().await {
        text.push_str(&decoder.push(&chunk));
    }
    text.push_str(&decoder.finish());
    text
}

/// One `fetch` with the response-header timeout.
async fn send_once(
    transport: &dyn Transport,
    request: &HttpRequest,
    timeout_ms: Option<u64>,
    signal: Option<&AbortSignal>,
) -> Result<HttpResponse, CodexError> {
    let timeout = async {
        match timeout_ms.filter(|t| *t > 0) {
            Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        sent = transport.send(request.clone()) => sent.map_err(CodexError::Plain),
        () = cancelled(signal) => Err(aborted_error()),
        () = timeout => Err(CodexError::Plain(format!(
            "Codex SSE response headers timed out after {}ms",
            timeout_ms.unwrap_or_default()
        ))),
    }
}

/// The SSE request with Codex's retry loop (`maxRetries` default 0, 1 s doubling, `retry-after` honored).
async fn send_with_codex_retry(
    transport: &dyn Transport,
    request: &HttpRequest,
    base: &StreamOptions,
    timeout_ms: Option<u64>,
    signal: Option<&AbortSignal>,
) -> Result<HttpResponse, CodexError> {
    let max_retries = base.max_retries.unwrap_or(0);
    let mut attempt: u32 = 0;
    loop {
        if is_aborted(signal) {
            return Err(aborted_error());
        }
        let outcome: Result<Option<HttpResponse>, CodexError> = async {
            let mut response = send_once(transport, request, timeout_ms, signal).await?;
            if (200..300).contains(&response.status) {
                return Ok(Some(response));
            }
            let text = read_text(&mut response).await;
            if attempt < max_retries && is_retryable_error(response.status, &text) {
                let delay = match retry_after_delay_ms(&response.headers) {
                    None => BASE_DELAY_MS * 2f64.powi(attempt as i32),
                    Some(delay) => validate_retry_delay_ms(delay, base.max_retry_delay_ms)?,
                };
                sleep(delay, signal).await?;
                return Ok(None);
            }
            let (message, friendly) = parse_error_response(response.status, &text, now_ms());
            Err(CodexError::Plain(friendly.unwrap_or(message)))
        }
        .await;
        match outcome {
            Ok(Some(response)) => return Ok(response),
            Ok(None) => {}
            Err(error) => {
                if error.message() == "Request was aborted" {
                    return Err(aborted_error());
                }
                let retryable = !matches!(error, CodexError::RetryDelay(_)) && !error.message().contains("usage limit");
                if attempt < max_retries && retryable {
                    sleep(BASE_DELAY_MS * 2f64.powi(attempt as i32), signal).await?;
                } else {
                    return Err(error);
                }
            }
        }
        attempt += 1;
    }
}

/// `streamSimple`: reasoning clamped to the model's levels and sent as `reasoningEffort`.
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
    let base = build_base_options(model, context, options);
    let effort = options
        .reasoning
        .as_deref()
        .map(|r| clamp_thinking_level(model, r))
        .filter(|e| *e != "off");
    let codex = OpenAICodexResponsesOptions {
        base,
        reasoning_effort: effort.map(str::to_string),
        tool_choice: options.tool_choice.clone(),
        ..Default::default()
    };
    Ok(stream(model, context, &codex, transport, sink).await)
}
