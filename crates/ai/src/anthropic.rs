//! Anthropic Messages provider (`api/anthropic-messages.ts`): request construction, the SSE event loop, usage and stop
//! reasons, retries and cancellation. HTTP goes through [`Transport`]; headers and URL follow the pinned
//! `@anthropic-ai/sdk` (0.124.0) minus its `x-stainless-*` telemetry headers.

use crate::constrained::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::http::{
    Failure, HeaderBuilder, HttpRequest, Read, Sdk, Transport, has_header, is_aborted, join_url, read_chunk,
    send_with_retry,
};
use crate::json::{parse_json_with_repair, parse_streaming_json};
use crate::models::calculate_cost;
use crate::options::{
    SimpleStreamOptions, StreamOptions, adjust_max_tokens_for_thinking, build_base_options,
    clamp_max_tokens_to_context, now_ms, pi_user_agent,
};
use crate::sse::{ServerSentEvent, SseDecoder};
use crate::transcript::{
    current_tools, declared_tools, has_tool_redefinitions, initial_system_message, render_system_message_update,
    resolve_transcript, system_message_text, transform_messages,
};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention, EventSink,
    Mapped, Message, Model, ModelCost, StopReason, TextContent, ThinkingContent, Tool, ToolCall, TranscriptContext,
    UserContent, UserMessageContent,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

const CLAUDE_CODE_VERSION: &str = "2.1.280";
const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const MID_CONVERSATION_TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";
const MESSAGE_EVENTS: &[&str] = &[
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

/// `AnthropicOptions`.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<u64>,
    /// `"low" | "medium" | "high" | "xhigh" | "max"`.
    pub effort: Option<String>,
    /// `"summarized" | "omitted"`.
    pub thinking_display: Option<String>,
    pub interleaved_thinking: Option<bool>,
    /// `"auto" | "any" | "none"` or `{type: "tool", name}`.
    pub tool_choice: Option<Value>,
}

struct Compat {
    eager_tool_input_streaming: bool,
    long_cache_retention: bool,
    session_affinity_headers: bool,
    openrouter_affinity: bool,
    cache_control_on_tools: bool,
    temperature: bool,
    allow_empty_signature: bool,
    strict_tools: bool,
    mid_convo_system_messages: bool,
    mid_convo_tool_changes: bool,
    managed_effort: bool,
    force_adaptive: bool,
}

impl Compat {
    fn of(model: &Model) -> Self {
        let openrouter = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
        let flag = |key: &str, default: bool| model.compat_bool(key).unwrap_or(default);
        Compat {
            eager_tool_input_streaming: flag("supportsEagerToolInputStreaming", true),
            long_cache_retention: flag("supportsLongCacheRetention", true),
            session_affinity_headers: flag("sendSessionAffinityHeaders", openrouter),
            openrouter_affinity: match model.compat_str("sessionAffinityFormat") {
                Some(format) => format == "openrouter",
                None => openrouter,
            },
            cache_control_on_tools: flag("supportsCacheControlOnTools", true),
            temperature: flag("supportsTemperature", true),
            allow_empty_signature: flag("allowEmptySignature", false),
            strict_tools: flag("supportsStrictTools", false),
            mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            mid_convo_tool_changes: flag("supportsMidConvoToolChanges", false),
            managed_effort: model.compat_bool("supportsMidConvoEffort") == Some(true),
            force_adaptive: model.compat_bool("forceAdaptiveThinking") == Some(true),
        }
    }
}

fn fallback_models(model: &Model) -> Vec<Value> {
    model
        .compat_value("allowedFallbackModels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn cache_control(model: &Model, options: &StreamOptions) -> Option<Value> {
    match options.cache_retention() {
        CacheRetention::None => None,
        CacheRetention::Long if Compat::of(model).long_cache_retention => {
            Some(json!({"type": "ephemeral", "ttl": "1h"}))
        }
        _ => Some(json!({"type": "ephemeral"})),
    }
}

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|t| t.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_string(), |t| t.to_string())
}

fn from_claude_code_name(name: &str, tools: &[Tool]) -> String {
    tools
        .iter()
        .find(|t| t.name.to_lowercase() == name.to_lowercase())
        .map_or_else(|| name.to_string(), |t| t.name.clone())
}

fn assert_request_auth(model: &Model, options: &StreamOptions) -> Result<(), String> {
    let headers = options.headers.as_ref();
    if options.api_key.as_ref().is_some_and(|k| !k.is_empty())
        || has_header(headers, "authorization")
        || has_header(headers, "x-api-key")
        || has_header(headers, "cf-aig-authorization")
    {
        return Ok(());
    }
    Err(format!("No API key for provider: {}", model.provider))
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn image_block(data: &str, mime_type: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data}})
}

/// Tool-result content: one string when there are no images, else blocks (with a placeholder when text is absent).
fn convert_content_blocks(content: &[UserContent]) -> Value {
    if !content.iter().any(|c| matches!(c, UserContent::Image(_))) {
        let text: Vec<&str> = content
            .iter()
            .map(|c| match c {
                UserContent::Text(t) => t.text.as_str(),
                UserContent::Image(_) => "",
            })
            .collect();
        return Value::String(text.join("\n"));
    }
    let mut blocks: Vec<Value> = content
        .iter()
        .map(|c| match c {
            UserContent::Text(t) => text_block(&t.text),
            UserContent::Image(i) => image_block(&i.data, &i.mime_type),
        })
        .collect();
    if !content.iter().any(|c| matches!(c, UserContent::Text(_))) {
        blocks.insert(0, text_block("(see attached image)"));
    }
    Value::Array(blocks)
}

fn copilot_dynamic_headers(messages: &[Message]) -> Vec<(String, String)> {
    let initiator = match messages.last() {
        Some(last) if !matches!(last, Message::User(_)) => "agent",
        _ => "user",
    };
    let mut headers = vec![
        ("X-Initiator".to_string(), initiator.to_string()),
        ("Openai-Intent".into(), "conversation-edits".into()),
    ];
    if has_vision_input(messages) {
        headers.push(("Copilot-Vision-Request".into(), "true".into()));
    }
    headers
}

pub(crate) fn has_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::User(u) => {
            matches!(&u.content, UserMessageContent::Blocks(b) if b.iter().any(|c| matches!(c, UserContent::Image(_))))
        }
        Message::ToolResult(r) => r.content.iter().any(|c| matches!(c, UserContent::Image(_))),
        _ => false,
    })
}

pub(crate) fn copilot_headers(messages: &[Message]) -> Vec<(String, String)> {
    copilot_dynamic_headers(messages)
}

/// Client default headers and whether the key is a Claude Code OAuth token.
fn client_headers(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    session_id: Option<&str>,
) -> (HeaderBuilder, bool) {
    let api_key = options.api_key.as_deref().filter(|k| !k.is_empty());
    let mut headers = HeaderBuilder::default();
    headers
        .set("Accept", "application/json")
        .set("anthropic-dangerous-direct-browser-access", "true")
        .set("anthropic-version", "2023-06-01");
    let oauth = model.provider != "github-copilot" && api_key.is_some_and(|k| k.contains("sk-ant-oat"));
    if model.provider == "github-copilot" || oauth {
        if let Some(key) = api_key {
            headers.set("Authorization", format!("Bearer {key}"));
        }
    } else if let Some(key) = api_key {
        headers.set("X-Api-Key", key);
    }
    headers
        .set("User-Agent", pi_user_agent())
        .set("accept", "application/json")
        .set("anthropic-dangerous-direct-browser-access", "true");
    if model.provider == "github-copilot" {
        headers.extend_set(model.headers.iter().flatten());
        for (name, value) in copilot_dynamic_headers(&context.messages) {
            headers.set(&name, value);
        }
    } else if oauth {
        headers
            .set("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
            .set("x-app", "cli");
        headers.extend_set(model.headers.iter().flatten());
    } else {
        let compat = Compat::of(model);
        if let (Some(session_id), true) = (session_id, compat.session_affinity_headers) {
            headers.set(
                if compat.openrouter_affinity {
                    "x-session-id"
                } else {
                    "x-session-affinity"
                },
                session_id,
            );
        }
        headers.extend_set(model.headers.iter().flatten());
    }
    headers.extend(options.headers.iter().flatten());
    headers.set("content-type", "application/json");
    (headers, oauth)
}

fn beta_features(
    model: &Model,
    context: &TranscriptContext,
    oauth: bool,
    native_tool_changes: bool,
    options: &AnthropicOptions,
) -> Vec<String> {
    let mut configured: Option<Option<String>> = None;
    for (name, value) in model.headers.iter().flatten() {
        if name.eq_ignore_ascii_case("anthropic-beta") {
            configured = Some(Some(value.clone()));
        }
    }
    for (name, value) in options.base.headers.iter().flatten() {
        if name.eq_ignore_ascii_case("anthropic-beta") {
            configured = Some(value.clone());
        }
    }
    let mut features: Vec<String> = Vec::new();
    match configured {
        Some(None) => return features,
        Some(Some(list)) => {
            for feature in list.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                features.push(feature.to_string());
            }
        }
        None => {
            let compat = Compat::of(model);
            if oauth {
                features.extend(["claude-code-20250219".to_string(), "oauth-2025-04-20".to_string()]);
            }
            if !current_tools(&context.messages).is_empty() && !compat.eager_tool_input_streaming {
                features.push(FINE_GRAINED_TOOL_STREAMING_BETA.into());
            }
            if model.reasoning
                && options.thinking_enabled == Some(true)
                && options.interleaved_thinking.unwrap_or(true)
                && !compat.force_adaptive
            {
                features.push(INTERLEAVED_THINKING_BETA.into());
            }
            if !fallback_models(model).is_empty() {
                features.push(SERVER_SIDE_FALLBACK_BETA.into());
            }
            if compat.managed_effort {
                features.extend([
                    MID_CONVERSATION_OUTPUT_CONFIG_BETA.to_string(),
                    THINKING_BINDING_CONTROLS_BETA.to_string(),
                ]);
            }
            if native_tool_changes {
                features.push(MID_CONVERSATION_TOOL_CHANGES_BETA.into());
            }
        }
    }
    let mut unique: Vec<String> = Vec::new();
    for feature in features {
        if !unique.contains(&feature) {
            unique.push(feature);
        }
    }
    unique
}

fn convert_tools(
    tools: &[Tool],
    oauth: bool,
    eager: bool,
    strict_tools: bool,
    cache_control: Option<&Value>,
) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let strict = resolve_json_schema_strict_sampling(tool, strict_tools)?;
        let parameters = json_schema_tool_parameters(tool, strict)?;
        let mut legacy = Map::new();
        legacy.insert("type".into(), json!("object"));
        legacy.insert(
            "properties".into(),
            parameters
                .get("properties")
                .filter(|p| !p.is_null())
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
        legacy.insert(
            "required".into(),
            parameters
                .get("required")
                .filter(|p| !p.is_null())
                .cloned()
                .unwrap_or_else(|| json!([])),
        );
        let input_schema = if strict == Some(true) {
            let mut merged = parameters.as_object().cloned().unwrap_or_default();
            merged.extend(legacy);
            merged
        } else {
            legacy
        };
        let mut converted = Map::new();
        converted.insert(
            "name".into(),
            json!(if oauth {
                to_claude_code_name(&tool.name)
            } else {
                tool.name.clone()
            }),
        );
        converted.insert("description".into(), json!(tool.description));
        if eager {
            converted.insert("eager_input_streaming".into(), json!(true));
        }
        if strict == Some(true) {
            converted.insert("strict".into(), json!(true));
        }
        converted.insert("input_schema".into(), Value::Object(input_schema));
        if let (Some(cache_control), true) = (cache_control, index + 1 == tools.len()) {
            converted.insert("cache_control".into(), cache_control.clone());
        }
        out.push(Value::Object(converted));
    }
    Ok(out)
}

const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Converted messages and the effort recorded on each managed assistant message, by output index.
fn convert_messages(
    messages: &[Message],
    oauth: bool,
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
    managed_provider: Option<&str>,
    native_tool_changes: bool,
) -> (Vec<Value>, Vec<(usize, String)>) {
    let mut params: Vec<Value> = Vec::new();
    let mut levels = Vec::new();
    let mut pending_system: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(system) => {
                let text = render_system_message_update(system);
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(text_block(&text));
                }
                if native_tool_changes {
                    let reference = |name: &str| json!({"type": "tool_reference", "name": if oauth { to_claude_code_name(name) } else { name.to_string() }});
                    for tool in system.tools_removed.iter().flatten() {
                        blocks.push(json!({"type": "tool_removal", "tool": reference(&tool.name)}));
                    }
                    for tool in system.tools_added.iter().flatten() {
                        blocks.push(json!({"type": "tool_addition", "tool": reference(&tool.name)}));
                    }
                }
                if !blocks.is_empty() {
                    pending_system.push(json!({"role": "system", "content": blocks}));
                }
            }
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => {
                    if !text.trim().is_empty() {
                        params.push(json!({"role": "user", "content": text}));
                    }
                }
                UserMessageContent::Blocks(blocks) => {
                    let blocks: Vec<Value> = blocks
                        .iter()
                        .filter(|b| !matches!(b, UserContent::Text(t) if t.text.trim().is_empty()))
                        .map(|b| match b {
                            UserContent::Text(t) => text_block(&t.text),
                            UserContent::Image(img) => image_block(&img.data, &img.mime_type),
                        })
                        .collect();
                    if !blocks.is_empty() {
                        params.push(json!({"role": "user", "content": blocks}));
                    }
                }
            },
            Message::Assistant(assistant) => {
                params.append(&mut pending_system);
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if !text.text.trim().is_empty() {
                                blocks.push(text_block(&text.text));
                            }
                        }
                        AssistantContent::Thinking(thinking) => {
                            if thinking.redacted == Some(true) {
                                blocks.push(json!({"type": "redacted_thinking", "data": thinking.thinking_signature.clone().unwrap_or_default()}));
                                continue;
                            }
                            let signature = thinking.thinking_signature.as_deref().filter(|s| !s.trim().is_empty());
                            if thinking.thinking.trim().is_empty() && signature.is_none() {
                                continue;
                            }
                            blocks.push(match signature {
                                Some(signature) => {
                                    json!({"type": "thinking", "thinking": thinking.thinking, "signature": signature})
                                }
                                None if allow_empty_signature => {
                                    json!({"type": "thinking", "thinking": thinking.thinking, "signature": ""})
                                }
                                None => text_block(&thinking.thinking),
                            });
                        }
                        AssistantContent::ToolCall(call) => blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": if oauth { to_claude_code_name(&call.name) } else { call.name.clone() },
                            "input": if call.arguments.is_null() { json!({}) } else { call.arguments.clone() },
                        })),
                    }
                }
                if blocks.is_empty() {
                    i += 1;
                    continue;
                }
                let index = params.len();
                params.push(json!({"role": "assistant", "content": blocks}));
                if let (Some(provider), Some(level)) = (managed_provider, assistant.provider_thinking_level.as_deref())
                    && assistant.api == "anthropic-messages"
                    && assistant.provider == provider
                    && EFFORTS.contains(&level)
                {
                    levels.push((index, level.to_string()));
                }
            }
            Message::ToolResult(_) => {
                let mut results = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(i) {
                    results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id,
                        "content": convert_content_blocks(&result.content),
                        "is_error": result.is_error,
                    }));
                    i += 1;
                }
                params.push(json!({"role": "user", "content": results}));
                continue;
            }
        }
        i += 1;
    }
    params.append(&mut pending_system);

    if let (Some(cache_control), Some(last)) = (cache_control, params.last_mut()) {
        let role = last.get("role").and_then(Value::as_str).unwrap_or_default();
        if role == "user" || role == "system" {
            match last.get_mut("content") {
                Some(Value::Array(blocks)) => {
                    if let Some(block) = blocks.last_mut() {
                        let kind = block.get("type").and_then(Value::as_str).unwrap_or_default();
                        if matches!(
                            kind,
                            "text" | "image" | "tool_result" | "tool_addition" | "tool_removal"
                        ) {
                            block["cache_control"] = cache_control.clone();
                        }
                    }
                }
                Some(content @ Value::String(_)) => {
                    let text = content.as_str().unwrap_or_default().to_string();
                    *content = json!([{"type": "text", "text": text, "cache_control": cache_control}]);
                }
                _ => {}
            }
        }
    }
    (params, levels)
}

fn normalize_tool_call_id(id: &str) -> String {
    let replaced: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    replaced.chars().take(64).collect()
}

fn deferred_tool_placeholder() -> Value {
    json!({
        "name": "__pi_deferred_placeholder__",
        "description": "Reserved placeholder. Never available. Never call this.",
        "input_schema": {"type": "object", "properties": {}, "required": []},
        "defer_loading": true,
    })
}

/// The request parameters (`buildParams`), `betas` included as upstream passes them to the SDK.
fn build_params(
    model: &Model,
    context: &TranscriptContext,
    oauth: bool,
    options: &AnthropicOptions,
    now: i64,
) -> Result<Map<String, Value>, String> {
    let cache_control = cache_control(model, &options.base);
    let compat = Compat::of(model);
    let initial = initial_system_message(&context.messages);
    let initial_text = initial.map(system_message_text).unwrap_or_default();
    let normalize = |id: &str, _: &AssistantMessage| normalize_tool_call_id(id);
    let transformed = transform_messages(&context.messages, model, Some(&normalize), now);
    let conversation = if initial.is_some() {
        &transformed[1..]
    } else {
        &transformed[..]
    };
    let initial_tools = initial.and_then(|m| m.tools_added.clone()).unwrap_or_default();
    let native_tool_changes = compat.mid_convo_system_messages
        && compat.mid_convo_tool_changes
        && !initial_tools.is_empty()
        && !has_tool_redefinitions(&context.messages);
    let (converted, levels) = convert_messages(
        conversation,
        oauth,
        cache_control.as_ref(),
        compat.allow_empty_signature,
        compat.managed_effort.then_some(model.provider.as_str()),
        native_tool_changes,
    );
    let active_effort = options.effort.clone().unwrap_or_else(|| "high".into());
    let betas = beta_features(model, context, oauth, native_tool_changes, options);
    let messages = if compat.managed_effort {
        let mut with_levels = Vec::with_capacity(converted.len() + levels.len() + 1);
        for (index, message) in converted.into_iter().enumerate() {
            if let Some((_, effort)) = levels.iter().find(|(i, _)| *i == index) {
                with_levels.push(json!({"role": "system", "content": [], "output_config": {"effort": effort}}));
            }
            with_levels.push(message);
        }
        with_levels.push(json!({"role": "system", "content": [], "output_config": {"effort": active_effort}}));
        with_levels
    } else {
        converted
    };

    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("messages".into(), Value::Array(messages));
    params.insert(
        "max_tokens".into(),
        json!(options.base.max_tokens.unwrap_or(model.max_tokens)),
    );
    params.insert("stream".into(), json!(true));
    if !betas.is_empty() {
        params.insert("betas".into(), json!(betas));
    }
    let system_block = |text: &str| {
        let mut block = Map::new();
        block.insert("type".into(), json!("text"));
        block.insert("text".into(), json!(text));
        if let Some(cache_control) = &cache_control {
            block.insert("cache_control".into(), cache_control.clone());
        }
        Value::Object(block)
    };
    if oauth {
        let mut system = vec![system_block(
            "You are Claude Code, Anthropic's official CLI for Claude.",
        )];
        if !initial_text.is_empty() {
            system.push(system_block(&initial_text));
        }
        params.insert("system".into(), Value::Array(system));
    } else if !initial_text.is_empty() {
        params.insert("system".into(), json!([system_block(&initial_text)]));
    }
    if let Some(temperature) = options.base.temperature
        && options.thinking_enabled != Some(true)
        && !compat.managed_effort
        && compat.temperature
    {
        params.insert("temperature".into(), json!(temperature));
    }
    let tool_cache_control = cache_control.as_ref().filter(|_| compat.cache_control_on_tools);
    if native_tool_changes {
        let initial_names: Vec<&str> = initial_tools.iter().map(|t| t.name.as_str()).collect();
        let later: Vec<Tool> = declared_tools(&context.messages)
            .into_iter()
            .filter(|t| !initial_names.contains(&t.name.as_str()))
            .collect();
        let mut tools = convert_tools(
            &initial_tools,
            oauth,
            compat.eager_tool_input_streaming,
            compat.strict_tools,
            tool_cache_control,
        )?;
        tools.push(deferred_tool_placeholder());
        for mut tool in convert_tools(
            &later,
            oauth,
            compat.eager_tool_input_streaming,
            compat.strict_tools,
            None,
        )? {
            tool["defer_loading"] = json!(true);
            tools.push(tool);
        }
        params.insert("tools".into(), Value::Array(tools));
    } else {
        let tools = current_tools(&context.messages);
        if !tools.is_empty() {
            let tools = convert_tools(
                &tools,
                oauth,
                compat.eager_tool_input_streaming,
                compat.strict_tools,
                tool_cache_control,
            )?;
            params.insert("tools".into(), Value::Array(tools));
        }
    }
    let display = options.thinking_display.clone().unwrap_or_else(|| "summarized".into());
    if compat.managed_effort {
        params.insert("thinking".into(), json!({"type": "adaptive", "display": display, "block_binding": {"prefix_mismatch_behavior": "drop_block"}}));
        params.insert("output_config".into(), json!({"effort": "high"}));
    } else if model.reasoning {
        if options.thinking_enabled == Some(true) {
            if compat.force_adaptive {
                params.insert("thinking".into(), json!({"type": "adaptive", "display": display}));
                if let Some(effort) = options.effort.as_deref().filter(|e| !e.is_empty()) {
                    params.insert("output_config".into(), json!({"effort": effort}));
                }
            } else {
                let budget = options.thinking_budget_tokens.filter(|b| *b > 0).unwrap_or(1024);
                params.insert(
                    "thinking".into(),
                    json!({"type": "enabled", "budget_tokens": budget, "display": display}),
                );
            }
        } else if options.thinking_enabled == Some(false) && model.thinking_level("off") != Mapped::Unsupported {
            params.insert("thinking".into(), json!({"type": "disabled"}));
        }
    }
    if let Some(Value::String(user_id)) = options.base.metadata.as_ref().and_then(|m| m.get("user_id")) {
        params.insert("metadata".into(), json!({"user_id": user_id}));
    }
    match &options.tool_choice {
        Some(Value::String(choice)) if !choice.is_empty() => {
            params.insert("tool_choice".into(), json!({"type": choice}));
        }
        Some(choice @ Value::Object(_)) => {
            params.insert("tool_choice".into(), choice.clone());
        }
        _ => {}
    }
    let fallbacks = fallback_models(model);
    if !fallbacks.is_empty() {
        let models: Vec<Value> = fallbacks
            .iter()
            .map(|f| json!({"model": f.get("model").cloned().unwrap_or(Value::Null)}))
            .collect();
        params.insert("fallbacks".into(), Value::Array(models));
    }
    Ok(params)
}

fn stop_reason(reason: &str, details: Option<&Value>) -> Result<(StopReason, Option<String>), String> {
    Ok(match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => {
            let explanation = details
                .and_then(|d| d.get("explanation"))
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty());
            (
                StopReason::Error,
                Some(
                    explanation
                        .unwrap_or("The model refused to complete the request")
                        .to_string(),
                ),
            )
        }
        "sensitive" => (StopReason::Error, Some("Provider stopped with: sensitive".into())),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

/// `x || 0` for a token count.
fn tokens(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

struct Run<'s, 'e> {
    output: AssistantMessage,
    sink: &'s mut EventSink<'e>,
    /// Upstream's per-block `index` (Anthropic content index), cleared on `content_block_stop`.
    indices: Vec<Option<u64>>,
    /// Upstream's per-tool-call `partialJson` scratch buffer.
    partial_json: Vec<String>,
    usage_cost: ModelCost,
    usage_model_id: String,
    transformations: Option<Vec<Value>>,
    saw_start: bool,
    saw_stop: bool,
}

impl Run<'_, '_> {
    fn emit(&mut self, event: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        let event = event(self.output.clone());
        (self.sink)(&event);
    }

    fn cost(&mut self, model: &Model) {
        let mut usage_model = model.clone();
        usage_model.cost = self.usage_cost.clone();
        usage_model.id = self.usage_model_id.clone();
        calculate_cost(&usage_model, &mut self.output.usage);
    }

    fn block_position(&self, index: Option<u64>) -> Option<usize> {
        index.and_then(|index| self.indices.iter().position(|i| *i == Some(index)))
    }

    fn push_block(&mut self, block: AssistantContent, index: Option<u64>) -> usize {
        self.output.content.push(block);
        self.indices.push(index);
        self.partial_json.push(String::new());
        self.output.content.len() - 1
    }

    fn handle_sse(&mut self, sse: &ServerSentEvent, model: &Model, oauth: bool, tools: &[Tool]) -> Result<(), String> {
        if sse.event.as_deref() == Some("error") {
            return Err(sse.data.clone());
        }
        if !MESSAGE_EVENTS.contains(&sse.event.as_deref().unwrap_or("")) {
            return Ok(());
        }
        let event = parse_json_with_repair(&sse.data).map_err(|e| {
            format!(
                "Could not parse Anthropic SSE event {}: {e}; data={}; raw={}",
                sse.event.as_deref().unwrap_or("null"),
                sse.data,
                sse.raw.join("\\n")
            )
        })?;
        self.handle_event(&event, model, oauth, tools)
    }

    fn handle_event(&mut self, event: &Value, model: &Model, oauth: bool, tools: &[Tool]) -> Result<(), String> {
        let index = event.get("index").and_then(Value::as_u64);
        match event.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => {
                self.saw_start = true;
                let message = &event["message"];
                self.output.response_id = message.get("id").and_then(Value::as_str).map(str::to_string);
                if let Some(Value::Array(t)) = message.get("input_transformations") {
                    self.transformations = Some(t.clone());
                }
                let response_model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let fallback_cost = if response_model == model.id {
                    None
                } else {
                    self.output.response_model = Some(response_model.clone());
                    fallback_models(model)
                        .into_iter()
                        .find(|f| {
                            f.get("provider").and_then(Value::as_str) == Some(&model.provider)
                                && f.get("model").and_then(Value::as_str) == Some(&response_model)
                        })
                        .and_then(|f| serde_json::from_value::<ModelCost>(f["cost"].clone()).ok())
                };
                (self.usage_cost, self.usage_model_id) = match fallback_cost {
                    Some(cost) => (cost, response_model),
                    None => (model.cost.clone(), model.id.clone()),
                };
                let usage = &message["usage"];
                let u = &mut self.output.usage;
                u.input = tokens(usage.get("input_tokens"));
                u.output = tokens(usage.get("output_tokens"));
                u.cache_read = tokens(usage.get("cache_read_input_tokens"));
                u.cache_write = tokens(usage.get("cache_creation_input_tokens"));
                u.cache_write_1h = Some(tokens(usage.pointer("/cache_creation/ephemeral_1h_input_tokens")));
                u.total_tokens = u.input + u.output + u.cache_read + u.cache_write;
                self.cost(model);
            }
            "message_stop" => self.saw_stop = true,
            "content_block_start" => {
                let block = &event["content_block"];
                let text = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                match block.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "fallback" => {
                        if !self.output.content.is_empty() {
                            return Err("Anthropic performed an unsupported mid-output model fallback".into());
                        }
                    }
                    "text" => {
                        let position = self.push_block(
                            AssistantContent::Text(TextContent {
                                text: text("text"),
                                text_signature: None,
                            }),
                            index,
                        );
                        self.emit(|partial| AssistantMessageEvent::TextStart {
                            content_index: position,
                            partial,
                        });
                    }
                    "thinking" => {
                        let thinking = ThinkingContent {
                            thinking: text("thinking"),
                            thinking_signature: Some(text("signature")),
                            redacted: None,
                        };
                        let position = self.push_block(AssistantContent::Thinking(thinking), index);
                        self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                            content_index: position,
                            partial,
                        });
                    }
                    "redacted_thinking" => {
                        let thinking = ThinkingContent {
                            thinking: "[Reasoning redacted]".into(),
                            thinking_signature: block.get("data").and_then(Value::as_str).map(str::to_string),
                            redacted: Some(true),
                        };
                        let position = self.push_block(AssistantContent::Thinking(thinking), index);
                        self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                            content_index: position,
                            partial,
                        });
                    }
                    "tool_use" => {
                        let name = text("name");
                        let call = ToolCall {
                            id: text("id"),
                            name: if oauth {
                                from_claude_code_name(&name, tools)
                            } else {
                                name
                            },
                            arguments: block
                                .get("input")
                                .filter(|i| !i.is_null())
                                .cloned()
                                .unwrap_or_else(|| json!({})),
                            thought_signature: None,
                            namespace: None,
                        };
                        let position = self.push_block(AssistantContent::ToolCall(call), index);
                        self.emit(|partial| AssistantMessageEvent::ToolCallStart {
                            content_index: position,
                            partial,
                        });
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(position) = self.block_position(index) else {
                    return Ok(());
                };
                let delta = &event["delta"];
                let field = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                match (
                    delta.get("type").and_then(Value::as_str).unwrap_or_default(),
                    &mut self.output.content[position],
                ) {
                    ("text_delta", AssistantContent::Text(block)) => {
                        let text = field("text");
                        block.text.push_str(&text);
                        self.emit(|partial| AssistantMessageEvent::TextDelta {
                            content_index: position,
                            delta: text,
                            partial,
                        });
                    }
                    ("thinking_delta", AssistantContent::Thinking(block)) => {
                        let thinking = field("thinking");
                        block.thinking.push_str(&thinking);
                        self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                            content_index: position,
                            delta: thinking,
                            partial,
                        });
                    }
                    ("input_json_delta", AssistantContent::ToolCall(call)) => {
                        let json = field("partial_json");
                        self.partial_json[position].push_str(&json);
                        call.arguments = parse_streaming_json(&self.partial_json[position]);
                        self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                            content_index: position,
                            delta: json,
                            partial,
                        });
                    }
                    ("signature_delta", AssistantContent::Thinking(block)) => {
                        block
                            .thinking_signature
                            .get_or_insert_with(String::new)
                            .push_str(&field("signature"));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let Some(position) = self.block_position(index) else {
                    return Ok(());
                };
                self.indices[position] = None;
                match &mut self.output.content[position] {
                    AssistantContent::Text(block) => {
                        let content = block.text.clone();
                        self.emit(|partial| AssistantMessageEvent::TextEnd {
                            content_index: position,
                            content,
                            partial,
                        });
                    }
                    AssistantContent::Thinking(block) => {
                        let content = block.thinking.clone();
                        self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                            content_index: position,
                            content,
                            partial,
                        });
                    }
                    AssistantContent::ToolCall(call) => {
                        call.arguments = parse_streaming_json(&self.partial_json[position]);
                        let tool_call = call.clone();
                        self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                            content_index: position,
                            tool_call,
                            partial,
                        });
                    }
                }
            }
            "message_delta" => {
                if let Some(Value::Array(t)) = event.get("input_transformations") {
                    self.transformations = Some(t.clone());
                }
                if let Some(reason) = event
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .filter(|r| !r.is_empty())
                {
                    self.output.raw_stop_reason = Some(reason.to_string());
                    let (stop, message) = stop_reason(reason, event.pointer("/delta/stop_details"))?;
                    self.output.stop_reason = stop;
                    if message.is_some() {
                        self.output.error_message = message;
                    }
                }
                if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
                    let u = &mut self.output.usage;
                    let set = |slot: &mut u64, key: &str| {
                        if let Some(v) = usage.get(key).and_then(Value::as_u64) {
                            *slot = v;
                        }
                    };
                    set(&mut u.input, "input_tokens");
                    set(&mut u.output, "output_tokens");
                    set(&mut u.cache_read, "cache_read_input_tokens");
                    set(&mut u.cache_write, "cache_creation_input_tokens");
                    if let Some(thinking) = usage
                        .pointer("/output_tokens_details/thinking_tokens")
                        .and_then(Value::as_u64)
                    {
                        u.reasoning = Some(thinking);
                    }
                }
                let u = &mut self.output.usage;
                u.total_tokens = u.input + u.output + u.cache_read + u.cache_write;
                self.cost(model);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Stream one Anthropic Messages response. Failures end the stream with an `error` event and an `error`/`aborted`
/// message, as upstream.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &AnthropicOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let compat = Compat::of(model);
    let context = resolve_transcript(context, compat.mid_convo_system_messages);
    let tools = current_tools(&context.messages);
    let now = now_ms();
    let mut run = Run {
        output: AssistantMessage::pending(model, now),
        sink,
        indices: Vec::new(),
        partial_json: Vec::new(),
        usage_cost: model.cost.clone(),
        usage_model_id: model.id.clone(),
        transformations: None,
        saw_start: false,
        saw_stop: false,
    };
    if compat.managed_effort {
        run.output.provider_thinking_level = Some(options.effort.clone().unwrap_or_else(|| "high".into()));
    }
    let signal = options.base.signal();
    let result = drive(&mut run, model, &context, options, transport, &tools, now).await;
    match result {
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
            run.output.error_message = Some(failure.message);
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
    options: &AnthropicOptions,
    transport: &dyn Transport,
    tools: &[Tool],
    now: i64,
) -> Result<(), Failure> {
    let base = &options.base;
    let signal = base.signal();
    assert_request_auth(model, base).map_err(Failure::plain)?;
    let session_id = if base.cache_retention() == CacheRetention::None {
        None
    } else {
        base.session_id.as_deref()
    };
    let (mut headers, oauth) = client_headers(model, context, base, session_id);
    let mut params = Value::Object(build_params(model, context, oauth, options, now).map_err(Failure::plain)?);
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&params, model)) {
        params = next;
        params["stream"] = json!(true);
    }
    if let Some(betas) = params.as_object_mut().and_then(|p| p.shift_remove("betas")) {
        let list: Vec<String> = betas
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| b.as_str().map_or_else(|| b.to_string(), str::to_string))
            .collect();
        headers.set("anthropic-beta", list.join(","));
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: join_url(&model.base_url, "/v1/messages?beta=true"),
        headers: headers.build(),
        body: serde_json::to_string(&params).unwrap_or_default(),
    };
    let mut response = send_with_retry(transport, &request, Sdk::Anthropic, base.retry(), signal).await?;
    run.emit(|partial| AssistantMessageEvent::Start { partial });

    let mut decoder = SseDecoder::default();
    loop {
        if is_aborted(signal) {
            return Err(Failure::plain("Request was aborted"));
        }
        match read_chunk(response.body.as_mut(), signal).await {
            Read::Chunk(bytes) => {
                for sse in decoder.push(&bytes) {
                    run.handle_sse(&sse, model, oauth, tools).map_err(Failure::plain)?;
                }
            }
            Read::End => break,
            Read::Aborted => return Err(Failure::plain("This operation was aborted")),
            Read::Failed(error) => return Err(Failure::plain(error)),
        }
    }
    for sse in decoder.finish(true) {
        run.handle_sse(&sse, model, oauth, tools).map_err(Failure::plain)?;
    }
    if run.saw_start && !run.saw_stop {
        return Err(Failure::plain("Anthropic stream ended before message_stop"));
    }
    if is_aborted(signal) {
        return Err(Failure::plain("Request was aborted"));
    }
    match run.output.stop_reason {
        StopReason::Pending => return Err(Failure::plain("Anthropic stream ended without a stop reason")),
        StopReason::Aborted | StopReason::Error => {
            let message = run.output.error_message.clone().filter(|m| !m.is_empty());
            return Err(Failure::plain(
                message.unwrap_or_else(|| "An unknown error occurred".into()),
            ));
        }
        _ => {}
    }
    if let Some(transformations) = run.transformations.take().filter(|t| !t.is_empty()) {
        let list: Vec<Value> = transformations
            .iter()
            .map(|t| {
                let mut entry = Map::new();
                for key in ["type", "path", "reason"] {
                    if let Some(value) = t.get(key).filter(|v| !v.is_null()) {
                        entry.insert(key.into(), value.clone());
                    }
                }
                Value::Object(entry)
            })
            .collect();
        let mut details = Map::new();
        details.insert("transformations".into(), Value::Array(list));
        run.output
            .diagnostics
            .get_or_insert_with(Vec::new)
            .push(AssistantMessageDiagnostic {
                kind: "anthropic_input_transformations".into(),
                timestamp: now_ms(),
                error: None,
                details: Some(details),
            });
    }
    Ok(())
}

fn effort_for_level(model: &Model, level: &str) -> String {
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

/// `streamSimple`: provider-neutral reasoning mapped to adaptive effort or a thinking budget. `Err` is upstream's
/// synchronous throw (missing auth), before any event.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    assert_request_auth(model, &options.base)?;
    let base = build_base_options(model, context, options);
    let mut anthropic = AnthropicOptions {
        tool_choice: options.tool_choice.clone().map(Value::String),
        ..AnthropicOptions {
            base,
            ..Default::default()
        }
    };
    match options.reasoning.as_deref() {
        None => anthropic.thinking_enabled = Some(false),
        Some(level) if model.compat_bool("forceAdaptiveThinking") == Some(true) => {
            anthropic.thinking_enabled = Some(true);
            anthropic.effort = Some(effort_for_level(model, level));
        }
        Some(level) => {
            let (adjusted, budget) = adjust_max_tokens_for_thinking(
                anthropic.base.max_tokens,
                model.max_tokens,
                level,
                options.thinking_budgets.as_ref(),
            );
            let max_tokens = clamp_max_tokens_to_context(model, context, adjusted);
            anthropic.base.max_tokens = Some(max_tokens);
            anthropic.thinking_enabled = Some(true);
            anthropic.thinking_budget_tokens = Some(budget.min(max_tokens.saturating_sub(1024)));
        }
    }
    Ok(stream(model, context, &anthropic, transport, sink).await)
}
