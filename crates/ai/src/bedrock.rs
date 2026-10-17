//! Amazon Bedrock Converse stream provider (`api/bedrock-converse-stream.ts`) without the AWS SDK: the same request
//! (schema member order of `ConverseStreamRequest`, blobs as base64), SigV4 or bearer-token auth, region and endpoint
//! resolution, the SDK's standard retry for throttling and transient failures, event-stream decoding, and the SDK's
//! error names and messages, formatted as upstream formats them.
//!
//! Credentials: explicit env keys, else the SDK default chain (`aws_credentials.rs`: env, shared files, assume role,
//! web identity, `credential_process`, SSO, container and instance metadata).

use crate::aws::{
    AwsCredentials, EventMessage, EventStreamDecoder, SignableRequest, backoff_delay_ms, escape_uri,
    is_throttling_error, is_transient_error, sign_request,
};
use crate::aws_credentials::{ChainInput, CredentialError, default_credentials, profile_config_value};
use crate::constrained::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::env::provider_env_value;
use crate::http::{AbortSignal, HttpRequest, HttpResponse, Read, Transport, cancelled, is_aborted, read_chunk};
use crate::json::{parse_json, parse_streaming_json};
use crate::jsutil::{atob, btoa, json_parse_error};
use crate::models::calculate_cost;
use crate::options::{
    SimpleStreamOptions, StreamOptions, ThinkingBudgets, adjust_max_tokens_for_thinking, build_base_options,
    clamp_max_tokens_to_context, clamp_reasoning, now_ms,
};
use crate::transcript::{
    collapse_system_messages, current_tools, initial_system_message, system_message_text, transform_messages,
};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention, EventSink,
    Mapped, Message, Model, StopReason, TextContent, ThinkingContent, Tool, ToolCall, TranscriptContext, UserContent,
    UserMessageContent,
};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::time::Duration;

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const DATA_RETENTION_DOCS_URL: &str = "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";
const DESERIALIZATION_HINT: &str =
    "\n  Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object.";
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// `BedrockOptions`.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub region: Option<String>,
    pub profile: Option<String>,
    /// `"auto" | "any" | "none" | { type: "tool", name }`.
    pub tool_choice: Option<Value>,
    pub reasoning: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub interleaved_thinking: Option<bool>,
    /// `"summarized" | "omitted"`.
    pub thinking_display: Option<String>,
    pub request_metadata: Option<IndexMap<String, String>>,
    pub bearer_token: Option<String>,
}

// ------------------------------------------------------------------------------------------------------- errors

/// A failure as upstream's error objects describe it.
#[derive(Debug, Default)]
struct BedrockError {
    name: String,
    message: String,
    /// A `BedrockRuntimeServiceException` (its name is mapped to a readable prefix).
    service: bool,
    /// `$metadata.httpStatusCode` / `$metadata.requestId`.
    status: Option<u16>,
    request_id: Option<String>,
    /// Classified as transient by the SDK retry strategy without a status (connection failures).
    transient: bool,
}

impl BedrockError {
    fn plain(message: impl Into<String>) -> Self {
        BedrockError {
            name: "Error".into(),
            message: message.into(),
            ..Default::default()
        }
    }

    fn retryable(&self) -> bool {
        self.transient || is_throttling_error(&self.name, self.status) || is_transient_error(&self.name, self.status)
    }

    fn throttling(&self) -> bool {
        is_throttling_error(&self.name, self.status)
    }
}

fn error_prefix(name: &str) -> &str {
    match name {
        "InternalServerException" => "Internal server error",
        "ModelStreamErrorException" => "Model stream error",
        "ValidationException" => "Validation error",
        "ThrottlingException" => "Throttling error",
        "ServiceUnavailableException" => "Service unavailable",
        other => other,
    }
}

/// `formatBedrockError`.
fn format_error(error: &BedrockError) -> String {
    let hint = if error.message.to_lowercase().contains("data retention mode") {
        format!(" See {DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        String::new()
    };
    if error.service {
        format!("{}: {}{hint}", error_prefix(&error.name), error.message)
    } else {
        format!("{}{hint}", error.message)
    }
}

const MAX_DIAGNOSTIC_VALUE_CHARS: usize = 200;

fn diagnostic_value(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty() && trimmed.encode_utf16().count() <= MAX_DIAGNOSTIC_VALUE_CHARS).then(|| trimmed.to_string())
}

/// `appendBedrockFailureDiagnostic`.
fn failure_diagnostic(error: &BedrockError, fallback_request_id: Option<&str>) -> Option<AssistantMessageDiagnostic> {
    let mut details = Map::new();
    if let Some(status) = error.status {
        details.insert("status".into(), json!(status));
    }
    if error.name.ends_with("Exception")
        && let Some(code) = diagnostic_value(Some(&error.name))
    {
        details.insert("errorCode".into(), json!(code));
    }
    if let Some(id) = diagnostic_value(error.request_id.as_deref()).or_else(|| fallback_request_id.map(str::to_string))
    {
        details.insert("requestId".into(), json!(id));
    }
    (!details.is_empty()).then(|| AssistantMessageDiagnostic {
        kind: "bedrock_response_failure".into(),
        timestamp: now_ms(),
        error: None,
        details: Some(details),
    })
}

/// `sanitizeErrorCode` + the `#` split of `getErrorSchemaOrThrowBaseException`.
fn sanitize_error_code(raw: &str) -> String {
    let mut code = raw;
    if let Some((first, _)) = code.split_once(',') {
        code = first;
    }
    if let Some((first, _)) = code.split_once(':') {
        code = first;
    }
    if let Some((_, after)) = code.split_once('#') {
        code = after.split('#').next().unwrap_or_default();
    }
    code.to_string()
}

fn request_id(headers: &crate::http::HeaderMap) -> Option<String> {
    ["x-amzn-requestid", "x-amzn-request-id", "x-amz-request-id"]
        .iter()
        .find_map(|name| headers.get(*name).cloned())
}

/// A non-2xx response as the restJson1 protocol turns it into an exception.
fn http_error(status: u16, headers: &crate::http::HeaderMap, body: &str) -> BedrockError {
    let request_id = request_id(headers);
    let data = if body.is_empty() {
        Value::Object(Map::new())
    } else {
        match parse_json(body) {
            Ok(value) => value,
            Err(_) => {
                return BedrockError {
                    name: "SyntaxError".into(),
                    message: format!("{}{DESERIALIZATION_HINT}", json_parse_error(body).unwrap_or_default()),
                    status: Some(status),
                    request_id,
                    ..Default::default()
                };
            }
        }
    };
    let object = data.as_object();
    let field = |key: &str| object.and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v));
    let code = if let Some(header) = headers.get("x-amzn-errortype") {
        sanitize_error_code(header)
    } else if let Some(code) = field("code").filter(|v| !v.is_null()) {
        sanitize_error_code(&value_text(code))
    } else if let Some(kind) = object.and_then(|o| o.get("__type")).filter(|v| !v.is_null()) {
        sanitize_error_code(&value_text(kind))
    } else {
        "Unknown".into()
    };
    let message = object
        .and_then(|o| {
            o.get("message")
                .filter(|v| !v.is_null())
                .or_else(|| o.get("Message").filter(|v| !v.is_null()))
        })
        .map(value_text)
        .unwrap_or_else(|| "UnknownError".into());
    BedrockError {
        name: code,
        message,
        service: true,
        status: Some(status),
        request_id,
        ..Default::default()
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => crate::json::stringify(other),
    }
}

/// Modeled stream exceptions (members of `ConverseStreamOutput`).
fn modeled_stream_exception(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "internalServerException" => "InternalServerException",
        "modelStreamErrorException" => "ModelStreamErrorException",
        "validationException" => "ValidationException",
        "throttlingException" => "ThrottlingException",
        "serviceUnavailableException" => "ServiceUnavailableException",
        _ => return None,
    })
}

/// An `exception` or `error` message of the event stream as the SDK throws it. `first` marks the message the SDK
/// reads while still inside the request (its errors get the deserialization hint and HTTP metadata).
fn stream_error(message: &EventMessage, first: bool, response_status: u16, response_id: Option<&str>) -> BedrockError {
    let header = |name: &str| message.headers.get(name).cloned().unwrap_or_default();
    let mut error = if header(":message-type") == "error" {
        let text = header(":error-message");
        BedrockError {
            name: header(":error-code"),
            message: if text.is_empty() { "UnknownError".into() } else { text },
            ..Default::default()
        }
    } else {
        let kind = header(":exception-type");
        match modeled_stream_exception(&kind) {
            Some(name) => {
                let body = parse_json(&String::from_utf8_lossy(&message.body)).unwrap_or(Value::Null);
                let text = body
                    .get("message")
                    .filter(|v| !v.is_null())
                    .or_else(|| body.get("Message").filter(|v| !v.is_null()))
                    .map(value_text)
                    .unwrap_or_else(|| "UnknownError".into());
                // Modeled exceptions carry SDK metadata without HTTP details: no hint, no status.
                return BedrockError {
                    name: name.into(),
                    message: text,
                    service: true,
                    ..Default::default()
                };
            }
            None => BedrockError {
                name: kind,
                message: String::from_utf8_lossy(&message.body).into_owned(),
                ..Default::default()
            },
        }
    };
    if first {
        error.message.push_str(DESERIALIZATION_HINT);
        error.status = Some(response_status);
        error.request_id = response_id.map(str::to_string);
    }
    error
}

// ------------------------------------------------------------------------------------------------ model predicates

fn match_candidates(model: &Model) -> Vec<String> {
    let mut values = vec![model.id.as_str()];
    if !model.name.is_empty() {
        values.push(&model.name);
    }
    values
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let mut dashed = String::new();
            let mut in_run = false;
            for c in lower.chars() {
                if c.is_whitespace() || matches!(c, '_' | '.' | ':') {
                    if !in_run {
                        dashed.push('-');
                    }
                    in_run = true;
                } else {
                    dashed.push(c);
                    in_run = false;
                }
            }
            [lower, dashed]
        })
        .collect()
}

fn any_candidate(model: &Model, needles: &[&str]) -> bool {
    match_candidates(model)
        .iter()
        .any(|c| needles.iter().any(|n| c.contains(n)))
}

/// `supportsAdaptiveThinking`.
pub fn supports_adaptive_thinking(model: &Model) -> bool {
    any_candidate(
        model,
        &[
            "opus-4-6",
            "opus-4-7",
            "opus-4-8",
            "opus-5",
            "sonnet-4-6",
            "sonnet-5",
            "haiku-5",
            "fable-5",
        ],
    )
}

fn supports_native_xhigh_effort(model: &Model) -> bool {
    any_candidate(
        model,
        &["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5"],
    )
}

/// `isAnthropicClaudeModel`.
pub fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

/// `supportsPromptCaching`.
pub fn supports_prompt_caching(model: &Model, env: Option<&crate::env::ProviderEnv>) -> bool {
    let candidates = match_candidates(model);
    if !candidates.iter().any(|c| c.contains("claude")) {
        return provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
    }
    candidates.iter().any(|c| {
        c.contains("fable-5")
            || c.contains("opus-5")
            || c.contains("sonnet-5")
            || c.contains("haiku-5")
            || c.contains("-4-")
            || c.contains("claude-3-7-sonnet")
            || c.contains("claude-3-5-haiku")
    })
}

fn effort_for(model: &Model, level: &str) -> String {
    if level == "xhigh" && supports_native_xhigh_effort(model) {
        return "xhigh".into();
    }
    if let Mapped::Value(mapped) = model.thinking_level(level) {
        return mapped.to_string();
    }
    match level {
        "minimal" | "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
    .into()
}

// ------------------------------------------------------------------------------------------------- request body

fn cache_retention(options: &StreamOptions) -> CacheRetention {
    options.cache_retention()
}

fn cache_point(retention: CacheRetention) -> Value {
    if retention == CacheRetention::Long {
        json!({"cachePoint": {"type": "default", "ttl": "1h"}})
    } else {
        json!({"cachePoint": {"type": "default"}})
    }
}

fn normalize_tool_call_id(id: &str, _source: &AssistantMessage) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let units: Vec<u16> = sanitized.encode_utf16().collect();
    if units.len() > 64 {
        String::from_utf16_lossy(&units[..64])
    } else {
        sanitized
    }
}

fn non_blank_text(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"text": text}))
}

fn required_text(text: &str) -> Value {
    non_blank_text(text).unwrap_or_else(|| json!({"text": EMPTY_TEXT_PLACEHOLDER}))
}

/// `atob` failure (`DOMException: The string to be decoded is not correctly encoded.`).
fn base64_bytes(data: &str) -> Result<Vec<u8>, String> {
    atob(data).ok_or_else(|| "The string to be decoded is not correctly encoded.".to_string())
}

fn image_block(mime_type: &str, data: &str) -> Result<Value, String> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => return Err(format!("Unknown image type: {other}")),
    };
    Ok(json!({"format": format, "source": {"bytes": btoa(&base64_bytes(data)?)}}))
}

fn sanitize_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_document).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.clone(), sanitize_document(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn tool_result_content(content: &[UserContent]) -> Result<Vec<Value>, String> {
    let mut result = Vec::new();
    for block in content {
        match block {
            UserContent::Image(image) => result.push(json!({"image": image_block(&image.mime_type, &image.data)?})),
            UserContent::Text(text) => result.extend(non_blank_text(&text.text)),
        }
    }
    if result.is_empty() {
        result.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
    }
    Ok(result)
}

fn convert_messages(
    context: &TranscriptContext,
    model: &Model,
    retention: CacheRetention,
    options: &StreamOptions,
) -> Result<Vec<Value>, String> {
    let messages: &[Message] = match context.messages.first() {
        Some(Message::System(_)) => &context.messages[1..],
        _ => &context.messages,
    };
    let transformed = transform_messages(messages, model, Some(&normalize_tool_call_id), now_ms());
    let claude = is_anthropic_claude_model(model);
    let mut result: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(message) => {
                let mut content = Vec::new();
                match &message.content {
                    UserMessageContent::Text(text) => content.push(required_text(text)),
                    UserMessageContent::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                UserContent::Text(text) => content.extend(non_blank_text(&text.text)),
                                UserContent::Image(image) => {
                                    content.push(json!({"image": image_block(&image.mime_type, &image.data)?}))
                                }
                            }
                        }
                        if content.is_empty() {
                            content.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
                        }
                    }
                }
                result.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant(message) => {
                let mut blocks = Vec::new();
                for block in &message.content {
                    match block {
                        AssistantContent::Text(text) => blocks.extend(non_blank_text(&text.text)),
                        AssistantContent::ToolCall(call) => blocks.push(json!({"toolUse": {
                            "toolUseId": call.id,
                            "name": call.name,
                            "input": sanitize_document(&call.arguments),
                        }})),
                        AssistantContent::Thinking(thinking) => {
                            if thinking.redacted == Some(true) {
                                let bytes = thinking
                                    .thinking_signature
                                    .as_deref()
                                    .filter(|s| !s.is_empty())
                                    .and_then(atob);
                                if let Some(bytes) = bytes.filter(|b| !b.is_empty()) {
                                    blocks.push(json!({"reasoningContent": {"redactedContent": btoa(&bytes)}}));
                                }
                                continue;
                            }
                            if thinking.thinking.trim().is_empty() {
                                continue;
                            }
                            if claude {
                                match thinking.thinking_signature.as_deref().filter(|s| !s.trim().is_empty()) {
                                    None => blocks.push(json!({"text": thinking.thinking})),
                                    Some(signature) => blocks.push(json!({"reasoningContent": {
                                        "reasoningText": {"text": thinking.thinking, "signature": signature}
                                    }})),
                                }
                            } else {
                                blocks
                                    .push(json!({"reasoningContent": {"reasoningText": {"text": thinking.thinking}}}));
                            }
                        }
                    }
                }
                if !blocks.is_empty() {
                    result.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Message::ToolResult(_) => {
                let mut results = Vec::new();
                while let Some(Message::ToolResult(next)) = transformed.get(i) {
                    results.push(json!({"toolResult": {
                        "toolUseId": next.tool_call_id,
                        "content": tool_result_content(&next.content)?,
                        "status": if next.is_error { "error" } else { "success" },
                    }}));
                    i += 1;
                }
                result.push(json!({"role": "user", "content": results}));
                continue;
            }
            Message::System(_) => {}
        }
        i += 1;
    }
    if retention != CacheRetention::None
        && supports_prompt_caching(model, options.env.as_ref())
        && let Some(last) = result.last_mut()
        && last["role"] == "user"
        && let Some(content) = last["content"].as_array_mut()
    {
        content.push(cache_point(retention));
    }
    Ok(result)
}

fn system_prompt(
    context: &TranscriptContext,
    model: &Model,
    retention: CacheRetention,
    options: &StreamOptions,
) -> Option<Value> {
    let text = initial_system_message(&context.messages).map(system_message_text)?;
    if text.is_empty() {
        return None;
    }
    let mut blocks = vec![json!({"text": text})];
    if retention != CacheRetention::None && supports_prompt_caching(model, options.env.as_ref()) {
        blocks.push(cache_point(retention));
    }
    Some(Value::Array(blocks))
}

fn tool_config(tools: &[Tool], choice: Option<&Value>, supports_strict_mode: bool) -> Result<Option<Value>, String> {
    if tools.is_empty() || choice.and_then(Value::as_str) == Some("none") {
        return Ok(None);
    }
    let mut specs = Vec::new();
    for tool in tools {
        let strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode)?;
        let mut spec = Map::new();
        spec.insert("name".into(), json!(tool.name));
        spec.insert(
            "inputSchema".into(),
            json!({"json": json_schema_tool_parameters(tool, strict)?}),
        );
        spec.insert("description".into(), json!(tool.description));
        if strict == Some(true) {
            spec.insert("strict".into(), json!(true));
        }
        specs.push(json!({"toolSpec": spec}));
    }
    let mut config = Map::new();
    config.insert("tools".into(), Value::Array(specs));
    let choice = match choice {
        Some(Value::String(s)) if s == "auto" => Some(json!({"auto": {}})),
        Some(Value::String(s)) if s == "any" => Some(json!({"any": {}})),
        Some(Value::Object(o)) if o.get("type").and_then(Value::as_str) == Some("tool") => {
            Some(json!({"tool": {"name": o.get("name").cloned().unwrap_or(Value::Null)}}))
        }
        _ => None,
    };
    if let Some(choice) = choice {
        config.insert("toolChoice".into(), choice);
    }
    Ok(Some(Value::Object(config)))
}

fn configured_region(options: &BedrockOptions) -> Option<String> {
    options
        .region
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| provider_env_value("AWS_REGION", options.base.env.as_ref()))
        .or_else(|| provider_env_value("AWS_DEFAULT_REGION", options.base.env.as_ref()))
}

fn is_govcloud(model: &Model, options: &BedrockOptions) -> bool {
    if configured_region(options).is_some_and(|r| r.to_lowercase().starts_with("us-gov-")) {
        return true;
    }
    let id = model.id.to_lowercase();
    id.starts_with("us-gov.") || id.starts_with("arn:aws-us-gov:")
}

fn additional_fields(model: &Model, options: &BedrockOptions) -> Option<Value> {
    let level = options.reasoning.as_deref()?;
    if !model.reasoning || !is_anthropic_claude_model(model) {
        return None;
    }
    let display = if is_govcloud(model, options) {
        None
    } else {
        Some(options.thinking_display.clone().unwrap_or_else(|| "summarized".into()))
    };
    let adaptive = supports_adaptive_thinking(model);
    let mut result = Map::new();
    if adaptive {
        let mut thinking = Map::new();
        thinking.insert("type".into(), json!("adaptive"));
        if let Some(display) = &display {
            thinking.insert("display".into(), json!(display));
        }
        result.insert("thinking".into(), Value::Object(thinking));
        result.insert("output_config".into(), json!({"effort": effort_for(model, level)}));
    } else {
        let budgets = options.thinking_budgets.unwrap_or_default();
        let custom = match level {
            "minimal" => budgets.minimal,
            "low" => budgets.low,
            "medium" => budgets.medium,
            _ => budgets.high,
        };
        let default = match level {
            "minimal" => 1024,
            "low" => 2048,
            "medium" => 8192,
            _ => 16384,
        };
        let mut thinking = Map::new();
        thinking.insert("type".into(), json!("enabled"));
        thinking.insert("budget_tokens".into(), json!(custom.unwrap_or(default)));
        if let Some(display) = &display {
            thinking.insert("display".into(), json!(display));
        }
        result.insert("thinking".into(), Value::Object(thinking));
        if options.interleaved_thinking.unwrap_or(true) {
            result.insert("anthropic_beta".into(), json!(["interleaved-thinking-2025-05-14"]));
        }
    }
    Some(Value::Object(result))
}

/// The `ConverseStreamCommand` input; the hook sees it with `modelId` first.
fn command_input(
    model: &Model,
    context: &TranscriptContext,
    options: &BedrockOptions,
) -> Result<Map<String, Value>, String> {
    let base = &options.base;
    let retention = cache_retention(base);
    let max_tokens = base
        .max_tokens
        .or(is_anthropic_claude_model(model).then_some(model.max_tokens));
    let mut inference = Map::new();
    if let Some(max_tokens) = max_tokens {
        inference.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = base.temperature {
        inference.insert("temperature".into(), json!(temperature));
    }
    let mut input = Map::new();
    input.insert("modelId".into(), json!(model.id));
    input.insert(
        "messages".into(),
        Value::Array(convert_messages(context, model, retention, base)?),
    );
    if let Some(system) = system_prompt(context, model, retention, base) {
        input.insert("system".into(), system);
    }
    input.insert("inferenceConfig".into(), Value::Object(inference));
    if let Some(config) = tool_config(
        &current_tools(&context.messages),
        options.tool_choice.as_ref(),
        model.compat_bool("supportsStrictMode").unwrap_or(false),
    )? {
        input.insert("toolConfig".into(), config);
    }
    if let Some(fields) = additional_fields(model, options) {
        input.insert("additionalModelRequestFields".into(), fields);
    }
    if let Some(metadata) = &options.request_metadata {
        input.insert("requestMetadata".into(), json!(metadata));
    }
    Ok(input)
}

/// The JSON body in `ConverseStreamRequest` member order.
const REQUEST_MEMBERS: &[&str] = &[
    "messages",
    "system",
    "inferenceConfig",
    "toolConfig",
    "guardrailConfig",
    "additionalModelRequestFields",
    "promptVariables",
    "additionalModelResponseFieldPaths",
    "requestMetadata",
    "performanceConfig",
    "serviceTier",
    "outputConfig",
];

// -------------------------------------------------------------------------------------------- endpoint and auth

fn standard_endpoint_region(base_url: &str) -> Option<String> {
    let host = reqwest::Url::parse(base_url).ok()?.host_str()?.to_lowercase();
    let rest = host
        .strip_prefix("bedrock-runtime-fips.")
        .or_else(|| host.strip_prefix("bedrock-runtime."))?;
    let region = rest
        .strip_suffix(".amazonaws.com")
        .or_else(|| rest.strip_suffix(".amazonaws.com.cn"))?;
    (!region.is_empty()
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    .then(|| region.to_string())
}

fn arn_region(model_id: &str) -> Option<String> {
    let rest = model_id.strip_prefix("arn:aws")?;
    let rest = match rest.strip_prefix('-') {
        Some(partition) => partition.split_once(':')?.1,
        None => rest.strip_prefix(':')?,
    };
    let rest = rest.strip_prefix("bedrock:")?;
    let (region, _) = rest.split_once(':')?;
    (!region.is_empty()
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    .then(|| region.to_string())
}

enum Auth {
    Bearer(String),
    SigV4(AwsCredentials),
    /// Resolved through the SDK default chain (for this profile) when the request is sent.
    Chain(Option<String>),
}

struct Target {
    endpoint: String,
    region: String,
    auth: Auth,
}

fn resolve_target(model: &Model, options: &BedrockOptions) -> Result<Target, BedrockError> {
    let env = options.base.env.as_ref();
    let options_profile = options.profile.clone().filter(|p| !p.is_empty()).or_else(|| {
        env.and_then(|e| e.get("AWS_PROFILE"))
            .filter(|p| !p.is_empty())
            .cloned()
    });
    let profile = options_profile
        .clone()
        .or_else(|| provider_env_value("AWS_PROFILE", env));
    let configured = configured_region(options);
    let ambient_profile = provider_env_value("AWS_PROFILE", None).is_some();
    let endpoint_region = standard_endpoint_region(&model.base_url);
    let explicit_endpoint = endpoint_region.is_none() || (configured.is_none() && !ambient_profile);
    let skip_auth = provider_env_value("AWS_BEDROCK_SKIP_AUTH", env).as_deref() == Some("1");
    let bearer = options
        .bearer_token
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| options.base.api_key.clone().filter(|t| !t.is_empty()))
        .or_else(|| provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env));
    let region = arn_region(&model.id)
        .or(configured)
        .or_else(|| endpoint_region.clone().filter(|_| explicit_endpoint))
        .or_else(|| (!ambient_profile).then(|| "us-east-1".to_string()))
        .or_else(|| profile_config_value(profile.as_deref().unwrap_or("default"), "region", env))
        .ok_or_else(|| BedrockError::plain("Region is missing"))?;
    let configured_endpoint = (!explicit_endpoint)
        .then(|| {
            configured_endpoint_url(&|name| provider_env_value(name, None), &|| {
                crate::aws_credentials::load_shared_files(env)
            })
        })
        .flatten();
    let endpoint = if explicit_endpoint {
        model.base_url.trim_end_matches('/').to_string()
    } else if let Some(url) = configured_endpoint {
        url.trim_end_matches('/').to_string()
    } else if region.starts_with("cn-") {
        format!("https://bedrock-runtime.{region}.amazonaws.com.cn")
    } else {
        format!("https://bedrock-runtime.{region}.amazonaws.com")
    };
    let auth = match bearer.filter(|_| !skip_auth) {
        Some(token) => Auth::Bearer(token),
        None if skip_auth => Auth::SigV4(AwsCredentials {
            access_key_id: "dummy-access-key".into(),
            secret_access_key: "dummy-secret-key".into(),
            session_token: None,
        }),
        None => match env_credentials(env).filter(|_| options_profile.is_none()) {
            Some(credentials) => Auth::SigV4(credentials),
            None => Auth::Chain(profile),
        },
    };
    Ok(Target { endpoint, region, auth })
}

/// The SDK's configured endpoint (`getEndpointFromConfig("Bedrock Runtime")`, used when the client sets no
/// `endpoint`): none when `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` (else the profile's `ignore_configured_endpoint_urls`)
/// is `true`; else `AWS_ENDPOINT_URL_BEDROCK_RUNTIME`, then `AWS_ENDPOINT_URL`, then the profile's `services`
/// section's `bedrock_runtime.endpoint_url`, then the profile's `endpoint_url`. `lookup` is the process environment
/// (`loadConfig` reads `process.env`); the profile is `AWS_PROFILE` or `default`, config file over credentials file.
/// A boolean that is neither `true` nor `false` is skipped, as `loadConfig` moves on to the next source.
pub fn configured_endpoint_url(
    lookup: &dyn Fn(&str) -> Option<String>,
    files: &dyn Fn() -> (crate::aws_credentials::Profiles, crate::aws_credentials::Profiles),
) -> Option<String> {
    let (config, credentials) = files();
    let name = lookup("AWS_PROFILE").unwrap_or_else(|| "default".into());
    let mut profile = credentials.get(&name).cloned().unwrap_or_default();
    for (key, value) in config.get(&name).cloned().unwrap_or_default() {
        profile.insert(key, value);
    }
    let boolean = |value: Option<String>| match value.as_deref() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    let ignore = boolean(lookup("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS"))
        .or_else(|| boolean(profile.get("ignore_configured_endpoint_urls").cloned()))
        .unwrap_or(false);
    if ignore {
        return None;
    }
    let set = |v: Option<String>| v.filter(|v| !v.is_empty());
    if let Some(url) = set(lookup("AWS_ENDPOINT_URL_BEDROCK_RUNTIME")).or_else(|| set(lookup("AWS_ENDPOINT_URL"))) {
        return Some(url);
    }
    if let Some(services) = profile.get("services") {
        // A missing services section fails this source; the default (none) follows.
        let section = config.get(&format!("services.{services}"))?;
        if let Some(url) = set(section.get("bedrock_runtime.endpoint_url").cloned()) {
            return Some(url);
        }
    }
    set(profile.get("endpoint_url").cloned())
}

fn env_credentials(env: Option<&crate::env::ProviderEnv>) -> Option<AwsCredentials> {
    Some(AwsCredentials {
        access_key_id: provider_env_value("AWS_ACCESS_KEY_ID", env)?,
        secret_access_key: provider_env_value("AWS_SECRET_ACCESS_KEY", env)?,
        session_token: provider_env_value("AWS_SESSION_TOKEN", env),
    })
}

/// The SDK default chain for the client's profile and region.
async fn chain_credentials(
    target: &Target,
    profile: Option<&str>,
    options: &BedrockOptions,
    transport: &dyn Transport,
) -> Result<AwsCredentials, BedrockError> {
    let input = ChainInput {
        env: options.base.env.as_ref(),
        transport,
        profile,
        region: &target.region,
        now_ms: now_ms(),
    };
    default_credentials(&input)
        .await
        .map_err(|error: CredentialError| BedrockError {
            name: error.name,
            message: error.message,
            status: error.status,
            ..Default::default()
        })
}

fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("x-amz-") || lower == "authorization" || lower == "host"
}

fn build_request(target: &Target, model: &Model, body: &str, options: &BedrockOptions) -> HttpRequest {
    let path_prefix = reqwest::Url::parse(&target.endpoint)
        .map(|u| u.path().trim_end_matches('/').to_string())
        .unwrap_or_default();
    let path = format!("{path_prefix}/model/{}/converse-stream", escape_uri(&model.id));
    let host = reqwest::Url::parse(&target.endpoint)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(port) => format!("{h}:{port}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_default();
    let origin = target.endpoint.strip_suffix(&path_prefix).unwrap_or(&target.endpoint);
    let mut headers: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    for (name, value) in options.base.headers.iter().flatten() {
        if let Some(value) = value
            && !is_reserved_header(name)
        {
            headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
            headers.push((name.to_ascii_lowercase(), value.clone()));
        }
    }
    match &target.auth {
        Auth::Bearer(token) => headers.push(("authorization".into(), format!("Bearer {token}"))),
        // Resolved by `drive` before the request is built.
        Auth::Chain(_) => {}
        Auth::SigV4(credentials) => {
            let mut signable = SignableRequest {
                method: "POST".into(),
                path: path.clone(),
                query: Vec::new(),
                headers: {
                    let mut all = headers.clone();
                    all.push(("host".into(), host));
                    all
                },
                body: body.as_bytes().to_vec(),
            };
            sign_request(&mut signable, credentials, &target.region, "bedrock", now_ms());
            headers = signable.headers;
        }
    }
    HttpRequest {
        method: "POST".into(),
        url: format!("{origin}{path}"),
        headers: headers.into_iter().collect(),
        body: body.to_string(),
    }
}

// ------------------------------------------------------------------------------------------------------ the stream

/// A content block being streamed, keyed by Bedrock's `contentBlockIndex`.
struct Block {
    index: Option<i64>,
    partial_json: Option<String>,
    redacted_chunks: Option<Vec<u8>>,
}

struct Run<'s, 'e> {
    output: AssistantMessage,
    blocks: Vec<Block>,
    sink: &'s mut EventSink<'e>,
}

impl Run<'_, '_> {
    fn emit(&mut self, event: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        let event = event(self.output.clone());
        (self.sink)(&event);
    }

    fn position(&self, index: i64) -> Option<usize> {
        self.blocks.iter().position(|b| b.index == Some(index))
    }

    fn finalize(&mut self, position: usize) {
        let block = &mut self.blocks[position];
        block.index = None;
        block.partial_json = None;
        if let Some(bytes) = block.redacted_chunks.take()
            && let AssistantContent::Thinking(thinking) = &mut self.output.content[position]
        {
            thinking.thinking_signature = Some(btoa(&bytes));
        }
    }

    fn finalize_all(&mut self) {
        for position in 0..self.blocks.len() {
            self.finalize(position);
        }
    }

    fn push(&mut self, content: AssistantContent, block: Block) -> usize {
        self.output.content.push(content);
        self.blocks.push(block);
        self.output.content.len() - 1
    }

    fn handle(&mut self, kind: &str, event: &Value, model: &Model) -> Result<(), BedrockError> {
        match kind {
            "messageStart" => {
                if event.get("role").and_then(Value::as_str) != Some("assistant") {
                    return Err(BedrockError::plain(
                        "Unexpected assistant message start but got user message start instead",
                    ));
                }
                self.emit(|partial| AssistantMessageEvent::Start { partial });
            }
            "contentBlockStart" => {
                let index = event
                    .get("contentBlockIndex")
                    .and_then(Value::as_i64)
                    .unwrap_or_default();
                if let Some(tool) = event.pointer("/start/toolUse") {
                    let text = |key: &str| tool.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                    let position = self.push(
                        AssistantContent::ToolCall(ToolCall {
                            id: text("toolUseId"),
                            name: text("name"),
                            arguments: json!({}),
                            thought_signature: None,
                            namespace: None,
                        }),
                        Block {
                            index: Some(index),
                            partial_json: Some(String::new()),
                            redacted_chunks: None,
                        },
                    );
                    self.emit(|partial| AssistantMessageEvent::ToolCallStart {
                        content_index: position,
                        partial,
                    });
                }
            }
            "contentBlockDelta" => self.delta(event)?,
            "contentBlockStop" => {
                let index = event
                    .get("contentBlockIndex")
                    .and_then(Value::as_i64)
                    .unwrap_or_default();
                let Some(position) = self.position(index) else {
                    return Ok(());
                };
                self.blocks[position].index = None;
                match &self.output.content[position] {
                    AssistantContent::Text(text) => {
                        let content = text.text.clone();
                        self.emit(|partial| AssistantMessageEvent::TextEnd {
                            content_index: position,
                            content,
                            partial,
                        });
                    }
                    AssistantContent::Thinking(_) => {
                        self.finalize(position);
                        let AssistantContent::Thinking(thinking) = &self.output.content[position] else {
                            unreachable!()
                        };
                        let content = thinking.thinking.clone();
                        self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                            content_index: position,
                            content,
                            partial,
                        });
                    }
                    AssistantContent::ToolCall(_) => {
                        let json_text = self.blocks[position].partial_json.take().unwrap_or_default();
                        let arguments = parse_streaming_json(&json_text);
                        let AssistantContent::ToolCall(call) = &mut self.output.content[position] else {
                            unreachable!()
                        };
                        call.arguments = arguments;
                        let tool_call = call.clone();
                        self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                            content_index: position,
                            tool_call,
                            partial,
                        });
                    }
                }
            }
            "messageStop" => {
                let reason = event.get("stopReason").and_then(Value::as_str);
                self.output.raw_stop_reason = reason.map(str::to_string);
                let (stop, message) = match reason {
                    Some("end_turn" | "stop_sequence") => (StopReason::Stop, None),
                    Some("max_tokens" | "model_context_window_exceeded") => (StopReason::Length, None),
                    Some("tool_use") => (StopReason::ToolUse, None),
                    Some(other) if !other.is_empty() => {
                        (StopReason::Error, Some(format!("Provider stopped with: {other}")))
                    }
                    _ => (StopReason::Error, None),
                };
                self.output.stop_reason = stop;
                if message.is_some() {
                    self.output.error_message = message;
                }
            }
            "metadata" => {
                if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
                    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
                    let out = &mut self.output.usage;
                    out.input = count("inputTokens");
                    out.output = count("outputTokens");
                    out.cache_read = count("cacheReadInputTokens");
                    out.cache_write = count("cacheWriteInputTokens");
                    out.cache_write_1h = usage.get("cacheDetails").and_then(Value::as_array).map(|details| {
                        details
                            .iter()
                            .filter(|d| d.get("ttl").and_then(Value::as_str) == Some("1h"))
                            .map(|d| d.get("inputTokens").and_then(Value::as_u64).unwrap_or(0))
                            .sum()
                    });
                    let total = count("totalTokens");
                    out.total_tokens = if total > 0 { total } else { out.input + out.output };
                    calculate_cost(model, out);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn delta(&mut self, event: &Value) -> Result<(), BedrockError> {
        let index = event
            .get("contentBlockIndex")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let Some(delta) = event.get("delta") else {
            return Ok(());
        };
        let mut position = self.position(index);
        if let Some(text) = delta.get("text").and_then(Value::as_str) {
            let position = match position {
                Some(p) => p,
                None => {
                    let p = self.push(
                        AssistantContent::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }),
                        Block {
                            index: Some(index),
                            partial_json: None,
                            redacted_chunks: None,
                        },
                    );
                    self.emit(|partial| AssistantMessageEvent::TextStart {
                        content_index: p,
                        partial,
                    });
                    p
                }
            };
            if let AssistantContent::Text(block) = &mut self.output.content[position] {
                block.text.push_str(text);
                let delta = text.to_string();
                self.emit(|partial| AssistantMessageEvent::TextDelta {
                    content_index: position,
                    delta,
                    partial,
                });
            }
        } else if let (Some(tool), Some(p)) = (delta.get("toolUse"), position) {
            if !matches!(self.output.content[p], AssistantContent::ToolCall(_)) {
                return Ok(());
            }
            let input = tool
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let buffer = self.blocks[p].partial_json.get_or_insert_with(String::new);
            buffer.push_str(&input);
            let arguments = parse_streaming_json(buffer);
            if let AssistantContent::ToolCall(call) = &mut self.output.content[p] {
                call.arguments = arguments;
            }
            self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                content_index: p,
                delta: input,
                partial,
            });
        } else if let Some(reasoning) = delta.get("reasoningContent").filter(|r| !r.is_null()) {
            if position.is_none() {
                let p = self.push(
                    AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: Some(String::new()),
                        redacted: None,
                    }),
                    Block {
                        index: Some(index),
                        partial_json: None,
                        redacted_chunks: None,
                    },
                );
                self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                    content_index: p,
                    partial,
                });
                position = Some(p);
            }
            let p = position.unwrap_or_default();
            if !matches!(self.output.content[p], AssistantContent::Thinking(_)) {
                return Ok(());
            }
            if let Some(text) = reasoning.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                if let AssistantContent::Thinking(block) = &mut self.output.content[p] {
                    block.thinking.push_str(text);
                }
                let delta = text.to_string();
                self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                    content_index: p,
                    delta,
                    partial,
                });
            }
            if let Some(signature) = reasoning
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                && let AssistantContent::Thinking(block) = &mut self.output.content[p]
                && block.redacted != Some(true)
            {
                block
                    .thinking_signature
                    .get_or_insert_with(String::new)
                    .push_str(signature);
            }
            if let Some(redacted) = reasoning.get("redactedContent").and_then(Value::as_str) {
                let bytes = atob(redacted).unwrap_or_default();
                if !bytes.is_empty() {
                    let first =
                        matches!(&self.output.content[p], AssistantContent::Thinking(b) if b.redacted != Some(true));
                    if first {
                        if let AssistantContent::Thinking(block) = &mut self.output.content[p] {
                            block.redacted = Some(true);
                            block.thinking_signature = Some(String::new());
                            block.thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
                        }
                        self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                            content_index: p,
                            delta: REDACTED_THINKING_PLACEHOLDER.into(),
                            partial,
                        });
                    }
                    self.blocks[p]
                        .redacted_chunks
                        .get_or_insert_with(Vec::new)
                        .extend(bytes);
                }
            }
        }
        Ok(())
    }
}

async fn read_body_text(response: &mut HttpResponse) -> String {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.body.next_chunk().await {
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn abort_error() -> BedrockError {
    BedrockError {
        name: "AbortError".into(),
        message: "Request aborted".into(),
        ..Default::default()
    }
}

/// An open event stream: the decoder with the messages read so far.
struct OpenStream {
    response: HttpResponse,
    decoder: EventStreamDecoder,
    pending: std::collections::VecDeque<EventMessage>,
    done: bool,
    status: u16,
    request_id: Option<String>,
}

impl OpenStream {
    async fn next(&mut self, signal: Option<&AbortSignal>) -> Result<Option<EventMessage>, BedrockError> {
        loop {
            if let Some(message) = self.pending.pop_front() {
                return Ok(Some(message));
            }
            if self.done {
                return Ok(None);
            }
            if is_aborted(signal) {
                return Err(abort_error());
            }
            match read_chunk(self.response.body.as_mut(), signal).await {
                Read::Chunk(bytes) => self
                    .pending
                    .extend(self.decoder.push(&bytes).map_err(BedrockError::plain)?),
                Read::End => {
                    self.done = true;
                    self.decoder.finish().map_err(BedrockError::plain)?;
                }
                Read::Aborted => return Err(abort_error()),
                Read::Failed(error) => return Err(BedrockError::plain(error)),
            }
        }
    }
}

/// One attempt: send, turn an error status into the protocol's exception, and read the first stream message (the
/// SDK reads it before `send` resolves, so its exceptions are retried like HTTP errors).
async fn attempt(
    transport: &dyn Transport,
    request: &HttpRequest,
    signal: Option<&AbortSignal>,
) -> Result<OpenStream, BedrockError> {
    if is_aborted(signal) {
        return Err(abort_error());
    }
    let sent = tokio::select! {
        sent = transport.send(request.clone()) => sent,
        () = cancelled(signal) => return Err(abort_error()),
    };
    let mut response = sent.map_err(|message| BedrockError {
        transient: true,
        ..BedrockError::plain(message)
    })?;
    let request_id = request_id(&response.headers);
    if !(200..300).contains(&response.status) {
        let text = read_body_text(&mut response).await;
        return Err(http_error(response.status, &response.headers, &text));
    }
    let status = response.status;
    let mut stream = OpenStream {
        response,
        decoder: EventStreamDecoder::default(),
        pending: Default::default(),
        done: false,
        status,
        request_id,
    };
    if let Some(first) = stream.next(signal).await? {
        if matches!(
            first.headers.get(":message-type").map(String::as_str),
            Some("exception" | "error")
        ) {
            return Err(stream_error(&first, true, stream.status, stream.request_id.as_deref()));
        }
        stream.pending.push_front(first);
    }
    Ok(stream)
}

fn max_attempts(options: &BedrockOptions) -> u32 {
    provider_env_value("AWS_MAX_ATTEMPTS", options.base.env.as_ref())
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

async fn send_with_retry(
    transport: &dyn Transport,
    request: &HttpRequest,
    options: &BedrockOptions,
) -> Result<OpenStream, BedrockError> {
    let signal = options.base.signal();
    let attempts = max_attempts(options);
    let mut retries = 0;
    loop {
        match attempt(transport, request, signal).await {
            Ok(stream) => return Ok(stream),
            Err(error) if retries + 1 < attempts && error.retryable() && !is_aborted(signal) => {
                let delay = backoff_delay_ms(retries, error.throttling());
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    () = cancelled(signal) => return Err(abort_error()),
                }
                retries += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Stream one Converse response; failures end with an `error` event and a `bedrock_response_failure` diagnostic.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &BedrockOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let context = collapse_system_messages(context);
    let mut output = AssistantMessage::pending(model, now_ms());
    output.api = "bedrock-converse-stream".into();
    let mut run = Run {
        output,
        blocks: Vec::new(),
        sink,
    };
    let mut response_id: Option<String> = None;
    let result = drive(&mut run, model, &context, options, transport, &mut response_id).await;
    let signal = options.base.signal();
    match result {
        Ok(()) => {
            run.finalize_all();
            let reason = run.output.stop_reason;
            run.emit(|message| AssistantMessageEvent::Done { reason, message });
        }
        Err(error) => {
            run.finalize_all();
            run.output.stop_reason = if is_aborted(signal) {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            run.output.error_message = Some(format_error(&error));
            if run.output.stop_reason == StopReason::Error
                && let Some(diagnostic) = failure_diagnostic(&error, response_id.as_deref())
            {
                run.output.diagnostics.get_or_insert_with(Vec::new).push(diagnostic);
            }
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
    options: &BedrockOptions,
    transport: &dyn Transport,
    response_id: &mut Option<String>,
) -> Result<(), BedrockError> {
    let signal = options.base.signal();
    let target = resolve_target(model, options)?;
    let mut input = Value::Object(command_input(model, context, options).map_err(BedrockError::plain)?);
    if let Some(next) = options.base.on_payload.as_ref().and_then(|hook| hook(&input, model)) {
        input = next;
    }
    let mut body = Map::new();
    for member in REQUEST_MEMBERS {
        if let Some(value) = input.get(*member).filter(|v| !v.is_null()) {
            body.insert((*member).to_string(), value.clone());
        }
    }
    let model_id = input
        .get("modelId")
        .and_then(Value::as_str)
        .unwrap_or(&model.id)
        .to_string();
    let request_model = Model {
        id: model_id,
        ..model.clone()
    };
    let mut target = target;
    if let Auth::Chain(profile) = &target.auth {
        let credentials = chain_credentials(&target, profile.as_deref(), options, transport).await?;
        target.auth = Auth::SigV4(credentials);
    }
    let request = build_request(
        &target,
        &request_model,
        &serde_json::to_string(&body).unwrap_or_default(),
        options,
    );
    // Through a proxy the SDK's HTTP/1.1 handler is used (its HTTP/2 handler takes no proxy agent); without one,
    // `AWS_BEDROCK_FORCE_HTTP1=1`: "Some custom endpoints require HTTP/1.1 instead of HTTP/2". The proxy itself is
    // the transport's.
    let proxied = crate::net::node_proxy_for_target(&model.base_url, options.base.env.as_ref())
        .map_err(BedrockError::plain)?
        .is_some();
    let http1 = (proxied
        || provider_env_value("AWS_BEDROCK_FORCE_HTTP1", options.base.env.as_ref()).as_deref() == Some("1"))
    .then(|| transport.http1_only())
    .flatten();
    let mut stream = send_with_retry(http1.as_deref().unwrap_or(transport), &request, options).await?;
    *response_id = diagnostic_value(stream.request_id.as_deref());
    while let Some(message) = stream.next(signal).await? {
        let header = |name: &str| message.headers.get(name).map(String::as_str);
        match header(":message-type") {
            Some("event") => {
                let kind = header(":event-type").unwrap_or_default().to_string();
                let event = parse_json(&String::from_utf8_lossy(&message.body)).unwrap_or(Value::Null);
                run.handle(&kind, &event, model)?;
            }
            Some("exception" | "error") => {
                return Err(stream_error(
                    &message,
                    false,
                    stream.status,
                    stream.request_id.as_deref(),
                ));
            }
            _ => {
                return Err(BedrockError::plain(format!(
                    "Unrecognizable event type: {}",
                    header(":event-type").unwrap_or("undefined")
                )));
            }
        }
    }
    if is_aborted(signal) {
        return Err(BedrockError::plain("Request was aborted"));
    }
    match run.output.stop_reason {
        StopReason::Pending => Err(BedrockError::plain("Bedrock stream ended without a stop reason")),
        StopReason::Error | StopReason::Aborted => Err(BedrockError::plain(
            run.output
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "An unknown error occurred".into()),
        )),
        _ => Ok(()),
    }
}

/// `streamSimple`: Claude budget models get their thinking budget fitted into `maxTokens`; adaptive Claude and other
/// models pass the level through.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let base = build_base_options(model, context, options);
    let tool_choice = options.tool_choice.clone().map(Value::String);
    let Some(level) = options.reasoning.clone() else {
        let bedrock = BedrockOptions {
            base,
            tool_choice,
            ..Default::default()
        };
        return stream(model, context, &bedrock, transport, sink).await;
    };
    let mut bedrock = BedrockOptions {
        base,
        tool_choice,
        reasoning: Some(level.clone()),
        thinking_budgets: options.thinking_budgets,
        ..Default::default()
    };
    if is_anthropic_claude_model(model) && !supports_adaptive_thinking(model) {
        let (adjusted, budget) = adjust_max_tokens_for_thinking(
            bedrock.base.max_tokens,
            model.max_tokens,
            &level,
            options.thinking_budgets.as_ref(),
        );
        let max_tokens = clamp_max_tokens_to_context(model, context, adjusted);
        bedrock.base.max_tokens = Some(max_tokens);
        let mut budgets = options.thinking_budgets.unwrap_or_default();
        let fitted = budget.min(max_tokens.saturating_sub(1024));
        match clamp_reasoning(&level) {
            "minimal" => budgets.minimal = Some(fitted),
            "low" => budgets.low = Some(fitted),
            "medium" => budgets.medium = Some(fitted),
            _ => budgets.high = Some(fitted),
        }
        bedrock.thinking_budgets = Some(budgets);
    }
    stream(model, context, &bedrock, transport, sink).await
}
