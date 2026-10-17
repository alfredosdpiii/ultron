//! OpenAI Chat Completions provider (`api/openai-completions.ts`), with upstream's URL/provider-based compat detection.
//! HTTP goes through [`Transport`]; headers and URL follow the pinned `openai` SDK (6.40.0) minus `x-stainless-*`.

use crate::anthropic::copilot_headers;
use crate::constrained::{
    GrammarInputBuffer, grammar_tool_input, grammar_tool_input_properties, json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use crate::http::{
    Failure, HeaderBuilder, HttpRequest, Read, Sdk, Transport, format_provider_error, has_header, is_aborted, join_url,
    read_chunk, send_with_retry,
};
use crate::json::{js_len, js_prefix, parse_json, parse_streaming_json, short_hash, stringify};
use crate::models::{calculate_cost, clamp_thinking_level};
use crate::options::{
    SimpleStreamOptions, StreamOptions, ThinkingBudgets, build_base_options, clamp_thinking_budget_to_answer_room,
    now_ms, pi_user_agent, thinking_budget_for_level,
};
use crate::sse::{ServerSentEvent, SseDecoder};
use crate::transcript::{
    declared_tools, render_system_message_update, resolve_transcript, resolve_transcript_tools, system_message_text,
    transform_messages,
};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, EventSink, Mapped, Message, Model,
    StopReason, TextContent, ThinkingContent, Tool, ToolCall, TranscriptContext, Usage, UserContent,
    UserMessageContent,
};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// `OpenAICompletionsOptions`.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompletionsOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    /// `ChatCompletionToolChoiceOption`: a string or an object.
    pub tool_choice: Option<Value>,
    /// `"minimal" | "low" | "medium" | "high" | "xhigh" | "max"`.
    pub reasoning_effort: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// Resolved compat (`getCompat`): detected from provider and URL, overridden by `model.compat`.
struct Compat {
    supports_store: bool,
    supports_developer_role: bool,
    supports_reasoning_effort: bool,
    supports_usage_in_streaming: bool,
    supports_finish_reason: bool,
    max_tokens_field: String,
    requires_tool_result_name: bool,
    requires_assistant_after_tool_result: bool,
    requires_thinking_as_text: bool,
    requires_reasoning_content_on_assistant_messages: bool,
    thinking_format: String,
    chat_template_kwargs: Map<String, Value>,
    chat_template_args: Map<String, Value>,
    zai_tool_stream: bool,
    supports_thinking_token_budget: bool,
    thinking_token_budget_field: Option<String>,
    supports_strict_mode: bool,
    supports_openai_grammar_tools: bool,
    supports_mid_convo_system_messages: bool,
    supports_mid_convo_tool_additions: bool,
    cache_control_format: Option<String>,
    send_session_affinity_headers: bool,
    session_affinity_format: String,
    supports_long_cache_retention: bool,
    vllm_priority: Option<Value>,
}

impl Compat {
    fn of(model: &Model) -> Self {
        let provider = model.provider.as_str();
        let url = model.base_url.as_str();
        let is_zai =
            matches!(provider, "zai" | "zai-coding-cn") || url.contains("api.z.ai") || url.contains("open.bigmodel.cn");
        let is_together = provider == "together" || url.contains("api.together.ai") || url.contains("api.together.xyz");
        let is_moonshot = matches!(provider, "moonshotai" | "moonshotai-cn") || url.contains("api.moonshot.");
        let is_openrouter = provider == "openrouter" || url.contains("openrouter.ai");
        let is_workers_ai = provider == "cloudflare-workers-ai" || url.contains("api.cloudflare.com");
        let is_ai_gateway = provider == "cloudflare-ai-gateway" || url.contains("gateway.ai.cloudflare.com");
        let is_nvidia = provider == "nvidia" || url.contains("integrate.api.nvidia.com");
        let is_ant_ling = provider == "ant-ling" || url.contains("api.ant-ling.com");
        let is_cerebras = provider == "cerebras" || url.contains("cerebras.ai");
        let is_deepseek = provider == "deepseek" || url.to_lowercase().contains("deepseek.com");
        let is_grok = provider == "xai" || url.contains("api.x.ai");
        let is_non_standard = is_nvidia
            || is_cerebras
            || is_grok
            || is_together
            || url.contains("chutes.ai")
            || is_deepseek
            || is_zai
            || is_moonshot
            || provider == "opencode"
            || url.contains("opencode.ai")
            || is_workers_ai
            || is_ai_gateway
            || is_ant_ling;
        let use_max_tokens = url.contains("chutes.ai")
            || is_deepseek
            || is_moonshot
            || is_ai_gateway
            || is_together
            || is_nvidia
            || is_ant_ling
            || is_zai;
        let developer_role_model =
            is_openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
        let thinking_format = if is_deepseek {
            "deepseek"
        } else if is_zai {
            "zai"
        } else if is_together {
            "together"
        } else if is_ant_ling {
            "ant-ling"
        } else if is_openrouter {
            "openrouter"
        } else {
            "openai"
        };
        let flag = |key: &str, detected: bool| model.compat_bool(key).unwrap_or(detected);
        let object = |key: &str| {
            model
                .compat_value(key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
        };
        Compat {
            supports_store: flag("supportsStore", !is_non_standard),
            supports_developer_role: flag(
                "supportsDeveloperRole",
                developer_role_model || (!is_non_standard && !is_openrouter),
            ),
            supports_reasoning_effort: flag(
                "supportsReasoningEffort",
                !is_grok && !is_zai && !is_moonshot && !is_together && !is_ai_gateway && !is_nvidia && !is_ant_ling,
            ),
            supports_usage_in_streaming: flag("supportsUsageInStreaming", true),
            supports_finish_reason: flag("supportsFinishReason", true),
            max_tokens_field: model
                .compat_str("maxTokensField")
                .unwrap_or(if use_max_tokens {
                    "max_tokens"
                } else {
                    "max_completion_tokens"
                })
                .to_string(),
            requires_tool_result_name: flag("requiresToolResultName", false),
            requires_assistant_after_tool_result: flag("requiresAssistantAfterToolResult", false),
            requires_thinking_as_text: flag("requiresThinkingAsText", false),
            requires_reasoning_content_on_assistant_messages: flag(
                "requiresReasoningContentOnAssistantMessages",
                is_deepseek,
            ),
            thinking_format: model
                .compat_str("thinkingFormat")
                .unwrap_or(thinking_format)
                .to_string(),
            chat_template_kwargs: object("chatTemplateKwargs"),
            chat_template_args: object("chatTemplateArgs"),
            zai_tool_stream: flag("zaiToolStream", false),
            supports_thinking_token_budget: flag("supportsThinkingTokenBudget", false),
            thinking_token_budget_field: model.compat_str("thinkingTokenBudgetField").map(str::to_string),
            supports_strict_mode: flag("supportsStrictMode", false),
            supports_openai_grammar_tools: flag("supportsOpenAIGrammarTools", false),
            supports_mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            supports_mid_convo_tool_additions: flag("supportsMidConvoToolAdditions", false),
            cache_control_format: model.compat_str("cacheControlFormat").map(str::to_string).or_else(|| {
                (provider == "openrouter" && model.id.starts_with("anthropic/")).then(|| "anthropic".into())
            }),
            send_session_affinity_headers: flag("sendSessionAffinityHeaders", is_openrouter),
            session_affinity_format: model
                .compat_str("sessionAffinityFormat")
                .unwrap_or(if is_openrouter { "openrouter" } else { "openai" })
                .to_string(),
            supports_long_cache_retention: flag(
                "supportsLongCacheRetention",
                !(is_together || is_workers_ai || is_ai_gateway || is_nvidia || is_ant_ling),
            ),
            vllm_priority: model.compat_value("vllmPriority").cloned(),
        }
    }
}

fn client_api_key(model: &Model, options: &StreamOptions) -> Result<String, String> {
    if let Some(key) = options.api_key.as_ref().filter(|k| !k.is_empty()) {
        return Ok(key.clone());
    }
    let headers = options.headers.as_ref();
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".into());
    }
    Err(format!("No API key for provider: {}", model.provider))
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::ToolResult(_) => true,
        Message::Assistant(a) => a.content.iter().any(|b| matches!(b, AssistantContent::ToolCall(_))),
        _ => false,
    })
}

/// A valid OpenRouter `reasoning_details` entry.
fn is_reasoning_detail(detail: &Value) -> bool {
    let Some(d) = detail.as_object() else { return false };
    let common = d.get("id").is_none_or(|v| v.is_null() || v.is_string())
        && d.get("format").is_none_or(Value::is_string)
        && d.get("index").is_none_or(Value::is_number);
    common
        && match d.get("type").and_then(Value::as_str) {
            Some("reasoning.summary") => d.get("summary").is_some_and(Value::is_string),
            Some("reasoning.encrypted") => d.get("data").is_some_and(Value::is_string),
            Some("reasoning.text") => {
                d.get("text").is_some_and(Value::is_string)
                    && d.get("signature").is_none_or(|s| s.is_null() || s.is_string())
            }
            _ => false,
        }
}

fn parse_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let parsed = parse_json(signature.filter(|s| !s.is_empty())?).ok()?;
    let items = parsed.as_array()?;
    (!items.is_empty() && items.iter().all(is_reasoning_detail)).then(|| items.clone())
}

fn parse_legacy_encrypted_detail(signature: Option<&str>) -> Option<Value> {
    let parsed = parse_json(signature.filter(|s| !s.is_empty())?).ok()?;
    let ok = is_reasoning_detail(&parsed)
        && parsed["type"] == "reasoning.encrypted"
        && parsed.get("id").and_then(Value::as_str).is_some_and(|i| !i.is_empty())
        && parsed
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|d| !d.is_empty());
    ok.then_some(parsed)
}

fn js_falsy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Bool(b)) => !b,
        _ => false,
    }
}

fn fill_common_fields(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    if target.get("id").is_none_or(Value::is_null)
        && let Some(id) = source.get("id")
    {
        target.insert("id".into(), id.clone());
    }
    if js_falsy(target.get("format"))
        && let Some(format) = source.get("format")
    {
        target.insert("format".into(), format.clone());
    }
    if target.get("index").is_none_or(Value::is_null)
        && let Some(index) = source.get("index")
    {
        target.insert("index".into(), index.clone());
    }
}

fn append_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let kind = detail["type"].as_str().unwrap_or_default();
    if let Some(Value::Object(last)) = details.last_mut()
        && last.get("type").and_then(Value::as_str) == Some(kind)
        && (kind == "reasoning.text" || kind == "reasoning.summary")
    {
        let source = detail.as_object().cloned().unwrap_or_default();
        let field = if kind == "reasoning.text" { "text" } else { "summary" };
        let joined = format!(
            "{}{}",
            last[field].as_str().unwrap_or_default(),
            source[field].as_str().unwrap_or_default()
        );
        last.insert(field.into(), json!(joined));
        if kind == "reasoning.text"
            && js_falsy(last.get("signature"))
            && let Some(signature) = source.get("signature")
        {
            last.insert("signature".into(), signature.clone());
        }
        fill_common_fields(last, &source);
        return;
    }
    details.push(detail.clone());
}

const REASONING_FIELDS: &[&str] = &["reasoning", "reasoning_content", "reasoning_text"];

fn normalize_tool_call_id(model: &Model, id: &str) -> String {
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    if let Some(separator) = id.find('|') {
        let call_id = sanitize(&id[..separator]);
        let item_id = sanitize(&id[separator + 1..]);
        let combined = if item_id.is_empty() {
            call_id.clone()
        } else {
            format!("{call_id}_{item_id}")
        };
        if js_len(&combined) <= 40 {
            return combined;
        }
        let hash: String = short_hash(id).chars().take(8).collect();
        let prefix = js_prefix(&call_id, 1.max(40 - js_len(&hash) - 1));
        return format!("{prefix}_{hash}");
    }
    if model.provider == "openai" && js_len(id) > 40 {
        return js_prefix(id, 40).to_string();
    }
    id.to_string()
}

fn image_url(data: &str, mime_type: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}})
}

fn convert_tools(tools: &[Tool], compat: &Compat) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        if let Some(grammar) = resolve_grammar_constrained_sampling(tool, compat.supports_openai_grammar_tools)? {
            out.push(json!({"type": "custom", "custom": {
                "name": tool.name,
                "description": tool.description,
                "format": {"type": "grammar", "grammar": {"syntax": grammar.format, "definition": grammar.definition}},
            }}));
            continue;
        }
        let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode)?;
        let mut function = Map::new();
        function.insert("name".into(), json!(tool.name));
        function.insert("description".into(), json!(tool.description));
        function.insert("parameters".into(), json_schema_tool_parameters(tool, strict)?);
        if compat.supports_strict_mode {
            function.insert("strict".into(), json!(strict.unwrap_or(false)));
        }
        out.push(json!({"type": "function", "function": function}));
    }
    Ok(out)
}

/// `convertMessages`: the transcript as Chat Completions messages.
fn convert_messages(
    model: &Model,
    context: &TranscriptContext,
    compat: &Compat,
    grammar: &IndexMap<String, String>,
    now: i64,
) -> Result<Vec<Value>, String> {
    let context = resolve_transcript(context, compat.supports_mid_convo_system_messages);
    let mut params: Vec<Value> = Vec::new();
    let normalize = |id: &str, _: &AssistantMessage| normalize_tool_call_id(model, id);
    let transformed = transform_messages(&context.messages, model, Some(&normalize), now);
    let tools = resolve_transcript_tools(
        &context.messages,
        compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_additions,
    );
    let instruction_role = if model.reasoning && compat.supports_developer_role {
        "developer"
    } else {
        "system"
    };
    let bridge = || json!({"role": "assistant", "content": "I have processed the tool results."});
    let mut last_role: Option<&str> = None;
    let mut i = 0;
    while i < transformed.len() {
        let message = &transformed[i];
        if compat.requires_assistant_after_tool_result
            && last_role == Some("toolResult")
            && matches!(message, Message::User(_))
        {
            params.push(bridge());
        }
        match message {
            Message::System(system) => {
                let added = if i > 0 && tools.anchors_additions {
                    system.tools_added.clone().unwrap_or_default()
                } else {
                    vec![]
                };
                if !added.is_empty() {
                    params.push(json!({"role": "system", "tools": convert_tools(&added, compat)?}));
                }
                let text = if i == 0 {
                    system_message_text(system)
                } else {
                    render_system_message_update(system)
                };
                if !text.is_empty() {
                    params.push(json!({"role": instruction_role, "content": text}));
                }
                last_role = Some("system");
            }
            Message::User(user) => {
                match &user.content {
                    UserMessageContent::Text(text) => params.push(json!({"role": "user", "content": text})),
                    UserMessageContent::Blocks(blocks) => {
                        let content: Vec<Value> = blocks
                            .iter()
                            .filter(|b| !matches!(b, UserContent::Text(t) if t.text.is_empty()))
                            .map(|b| match b {
                                UserContent::Text(t) => json!({"type": "text", "text": t.text}),
                                UserContent::Image(img) => image_url(&img.data, &img.mime_type),
                            })
                            .collect();
                        if content.is_empty() {
                            i += 1;
                            continue;
                        }
                        params.push(json!({"role": "user", "content": content}));
                    }
                }
                last_role = Some("user");
            }
            Message::Assistant(assistant) => {
                let mut out = Map::new();
                out.insert("role".into(), json!("assistant"));
                out.insert(
                    "content".into(),
                    if compat.requires_assistant_after_tool_result {
                        json!("")
                    } else {
                        Value::Null
                    },
                );
                let text_parts: Vec<&str> = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantContent::Text(t) if !t.text.trim().is_empty() => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let assistant_text = text_parts.concat();
                let thinking: Vec<&ThinkingContent> = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantContent::Thinking(t) => Some(t),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<&ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantContent::ToolCall(c) => Some(c),
                        _ => None,
                    })
                    .collect();
                let signed = thinking
                    .iter()
                    .find_map(|t| parse_reasoning_details(t.thinking_signature.as_deref()));
                let legacy: Vec<Value> = calls
                    .iter()
                    .filter_map(|c| parse_legacy_encrypted_detail(c.thought_signature.as_deref()))
                    .collect();
                let preserved = signed.or_else(|| (!legacy.is_empty()).then_some(legacy));
                let non_empty: Vec<&&ThinkingContent> =
                    thinking.iter().filter(|t| !t.thinking.trim().is_empty()).collect();
                if !non_empty.is_empty() {
                    if compat.requires_thinking_as_text {
                        let thinking_text = non_empty
                            .iter()
                            .map(|t| t.thinking.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let mut parts = vec![json!({"type": "text", "text": thinking_text})];
                        parts.extend(text_parts.iter().map(|t| json!({"type": "text", "text": t})));
                        out.insert("content".into(), Value::Array(parts));
                    } else {
                        if !assistant_text.is_empty() {
                            out.insert("content".into(), json!(assistant_text));
                        }
                        if preserved.is_none() {
                            let mut signature = non_empty[0].thinking_signature.clone();
                            if model.provider == "opencode-go" && signature.as_deref() == Some("reasoning") {
                                signature = Some("reasoning_content".into());
                            }
                            if let Some(field) = signature.filter(|s| REASONING_FIELDS.contains(&s.as_str())) {
                                let joined = non_empty
                                    .iter()
                                    .map(|t| t.thinking.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                out.insert(field, json!(joined));
                            }
                        }
                    }
                } else if !assistant_text.is_empty() {
                    out.insert("content".into(), json!(assistant_text));
                }
                if !calls.is_empty() {
                    let mut tool_calls = Vec::new();
                    for call in &calls {
                        tool_calls.push(match grammar.get(&call.name) {
                            Some(property) => json!({"id": call.id, "type": "custom", "custom": {
                                "name": call.name,
                                "input": grammar_tool_input(&call.name, &call.arguments, property)?,
                            }}),
                            None => json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": stringify(&call.arguments)}}),
                        });
                    }
                    out.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                if let Some(details) = preserved {
                    out.insert("reasoning_details".into(), Value::Array(details));
                }
                if compat.requires_reasoning_content_on_assistant_messages
                    && model.reasoning
                    && !out.contains_key("reasoning_content")
                {
                    out.insert("reasoning_content".into(), json!(""));
                }
                let has_content = match out.get("content") {
                    Some(Value::String(s)) => !s.is_empty(),
                    Some(Value::Array(a)) => !a.is_empty(),
                    _ => false,
                };
                if !has_content && !out.contains_key("tool_calls") {
                    i += 1;
                    continue;
                }
                params.push(Value::Object(out));
                last_role = Some("assistant");
            }
            Message::ToolResult(_) => {
                let mut images = Vec::new();
                while let Some(Message::ToolResult(result)) = transformed.get(i) {
                    let text = result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            UserContent::Text(t) => Some(t.text.as_str()),
                            UserContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let has_images = result.content.iter().any(|c| matches!(c, UserContent::Image(_)));
                    let content = if !text.is_empty() {
                        text
                    } else if has_images {
                        "(see attached image)".into()
                    } else {
                        "(no tool output)".into()
                    };
                    let mut out = Map::new();
                    out.insert("role".into(), json!("tool"));
                    out.insert("content".into(), json!(content));
                    out.insert("tool_call_id".into(), json!(result.tool_call_id));
                    if compat.requires_tool_result_name && !result.tool_name.is_empty() {
                        out.insert("name".into(), json!(result.tool_name));
                    }
                    params.push(Value::Object(out));
                    if has_images && model.supports_images() {
                        for block in &result.content {
                            if let UserContent::Image(img) = block {
                                images.push(image_url(&img.data, &img.mime_type));
                            }
                        }
                    }
                    i += 1;
                }
                if images.is_empty() {
                    last_role = Some("toolResult");
                } else {
                    if compat.requires_assistant_after_tool_result {
                        params.push(bridge());
                    }
                    let mut content = vec![json!({"type": "text", "text": "Attached image(s) from tool result:"})];
                    content.extend(images);
                    params.push(json!({"role": "user", "content": content}));
                    last_role = Some("user");
                }
                continue;
            }
        }
        i += 1;
    }
    Ok(params)
}

/// Mark the last text part (or a non-empty string) of `message` with `cache_control`.
fn add_cache_control_to_text(message: &mut Value, cache_control: &Value) -> bool {
    match message.get_mut("content") {
        Some(content @ Value::String(_)) => {
            let text = content.as_str().unwrap_or_default().to_string();
            if text.is_empty() {
                return false;
            }
            *content = json!([{"type": "text", "text": text, "cache_control": cache_control}]);
            true
        }
        Some(Value::Array(parts)) => match parts
            .iter_mut()
            .rev()
            .find(|p| p.get("type").and_then(Value::as_str) == Some("text"))
        {
            Some(part) => {
                part["cache_control"] = cache_control.clone();
                true
            }
            None => false,
        },
        _ => false,
    }
}

fn apply_anthropic_cache_control(messages: &mut [Value], tools: Option<&mut Value>, cache_control: &Value) {
    let role = |m: &Value| m.get("role").and_then(Value::as_str).unwrap_or_default().to_string();
    if let Some(system) = messages
        .iter_mut()
        .find(|m| matches!(role(m).as_str(), "system" | "developer"))
    {
        add_cache_control_to_text(system, cache_control);
    }
    if let Some(Value::Array(tools)) = tools
        && let Some(last) = tools.last_mut()
    {
        last["cache_control"] = cache_control.clone();
    }
    for message in messages.iter_mut().rev() {
        if matches!(role(message).as_str(), "user" | "assistant" | "tool")
            && add_cache_control_to_text(message, cache_control)
        {
            return;
        }
    }
}

fn chat_template_values(
    model: &Model,
    effort: Option<&str>,
    values: &Map<String, Value>,
    budget: Option<u64>,
) -> Option<Map<String, Value>> {
    let mut resolved = Map::new();
    for (key, value) in values {
        let Some(spec) = value.as_object() else {
            resolved.insert(key.clone(), value.clone());
            continue;
        };
        if effort.is_none() && spec.get("omitWhenOff").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let value = match spec.get("$var").and_then(Value::as_str) {
            Some("thinking.enabled") => Some(json!(effort.is_some())),
            Some("thinking.budget") => budget.map(|b| json!(b)),
            _ => match model.thinking_level(effort.unwrap_or("off")) {
                Mapped::Missing => effort.map(|e| json!(e)),
                Mapped::Value(mapped) => Some(json!(mapped)),
                Mapped::Unsupported => None,
            },
        };
        if let Some(value) = value {
            resolved.insert(key.clone(), value);
        }
    }
    (!resolved.is_empty()).then_some(resolved)
}

fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICompletionsOptions,
    compat: &Compat,
    retention: CacheRetention,
    grammar: &IndexMap<String, String>,
    now: i64,
) -> Result<Map<String, Value>, String> {
    let base = &options.base;
    let tools = resolve_transcript_tools(
        &context.messages,
        compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_additions,
    );
    let mut messages = convert_messages(model, context, compat, grammar, now)?;
    let cache_control =
        (compat.cache_control_format.as_deref() == Some("anthropic") && retention != CacheRetention::None).then(|| {
            if retention == CacheRetention::Long && compat.supports_long_cache_retention {
                json!({"type": "ephemeral", "ttl": "1h"})
            } else {
                json!({"type": "ephemeral"})
            }
        });
    let long = retention == CacheRetention::Long && compat.supports_long_cache_retention;
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("messages".into(), Value::Null);
    params.insert("stream".into(), json!(true));
    if ((model.base_url.contains("api.openai.com") && retention != CacheRetention::None) || long)
        && let Some(session) = &base.session_id
    {
        params.insert(
            "prompt_cache_key".into(),
            json!(session.chars().take(64).collect::<String>()),
        );
    }
    if long {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if compat.supports_usage_in_streaming {
        params.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if compat.supports_store {
        params.insert("store".into(), json!(false));
    }
    if let Some(max_tokens) = base.max_tokens.filter(|m| *m > 0) {
        let field = if compat.max_tokens_field == "max_tokens" {
            "max_tokens"
        } else {
            "max_completion_tokens"
        };
        params.insert(field.into(), json!(max_tokens));
    }
    if let Some(temperature) = base.temperature {
        params.insert("temperature".into(), json!(temperature));
    }
    let mut tools_value = if !tools.request_tools.is_empty() {
        Some(Value::Array(convert_tools(&tools.request_tools, compat)?))
    } else if has_tool_history(&context.messages) {
        Some(json!([]))
    } else {
        None
    };
    if let Some(cache_control) = &cache_control {
        apply_anthropic_cache_control(&mut messages, tools_value.as_mut(), cache_control);
    }
    params.insert("messages".into(), Value::Array(messages));
    if let Some(tools_value) = tools_value {
        let has_tools = tools_value.as_array().is_some_and(|t| !t.is_empty());
        params.insert("tools".into(), tools_value);
        if has_tools && compat.zai_tool_stream {
            params.insert("tool_stream".into(), json!(true));
        }
    }
    if let Some(choice) = options.tool_choice.as_ref().filter(|c| !js_falsy(Some(c))) {
        params.insert("tool_choice".into(), choice.clone());
    }
    if let Some(priority) = &compat.vllm_priority {
        params.insert("priority".into(), priority.clone());
    }
    let budget_field = compat.thinking_token_budget_field.clone().or_else(|| {
        compat
            .supports_thinking_token_budget
            .then(|| "thinking_token_budget".to_string())
    });
    let effort = options.reasoning_effort.as_deref().filter(|e| !e.is_empty());
    let budget = match effort {
        Some(effort) if model.reasoning => {
            let ceiling = params
                .get("max_tokens")
                .or_else(|| params.get("max_completion_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(model.max_tokens);
            Some(clamp_thinking_budget_to_answer_room(
                thinking_budget_for_level(effort, options.thinking_budgets.as_ref()),
                ceiling,
            ))
            .filter(|b| *b > 0)
        }
        _ => None,
    };
    let off = model.thinking_level("off");
    let off_or_none = || match off {
        Mapped::Value(v) => v.to_string(),
        _ => "none".to_string(),
    };
    let format = compat.thinking_format.as_str();
    let reasoning = model.reasoning;
    if format == "zai" && reasoning {
        params.insert(
            "thinking".into(),
            if effort.is_some() {
                json!({"type": "enabled", "clear_thinking": false})
            } else {
                json!({"type": "disabled"})
            },
        );
        if let (Some(effort), true) = (effort, compat.supports_reasoning_effort) {
            match model.thinking_level(effort) {
                Mapped::Missing => {
                    params.insert("reasoning_effort".into(), json!(effort));
                }
                Mapped::Value(mapped) => {
                    params.insert("reasoning_effort".into(), json!(mapped));
                }
                Mapped::Unsupported => {}
            }
        }
    } else if format == "qwen" && reasoning {
        params.insert("enable_thinking".into(), json!(effort.is_some()));
        if let (Some(effort), true) = (effort, compat.supports_reasoning_effort) {
            params.insert("reasoning_effort".into(), json!(model.mapped_or(effort)));
        }
    } else if format == "qwen-chat-template" && reasoning {
        params.insert(
            "chat_template_kwargs".into(),
            json!({"enable_thinking": effort.is_some(), "preserve_thinking": true}),
        );
    } else if format == "chat-template" && reasoning {
        if let Some(values) = chat_template_values(model, effort, &compat.chat_template_kwargs, budget) {
            params.insert("chat_template_kwargs".into(), Value::Object(values));
        }
    } else if format == "baseten" && reasoning {
        if let Some(values) = chat_template_values(model, effort, &compat.chat_template_args, budget) {
            params.insert("chat_template_args".into(), Value::Object(values));
        }
        if compat.supports_reasoning_effort {
            let value = match model.thinking_level(effort.unwrap_or("off")) {
                Mapped::Missing => effort.map(str::to_string),
                Mapped::Value(mapped) => Some(mapped.to_string()),
                Mapped::Unsupported => None,
            };
            if let Some(value) = value {
                params.insert("reasoning_effort".into(), json!(value));
            }
        }
    } else if format == "deepseek" && reasoning {
        if effort.is_some() {
            params.insert("thinking".into(), json!({"type": "enabled"}));
        } else if off != Mapped::Unsupported {
            params.insert("thinking".into(), json!({"type": "disabled"}));
        }
        if let (Some(effort), true) = (effort, compat.supports_reasoning_effort) {
            params.insert("reasoning_effort".into(), json!(model.mapped_or(effort)));
        }
    } else if format == "openrouter" && reasoning {
        if let Some(effort) = effort {
            params.insert("reasoning".into(), json!({"effort": model.mapped_or(effort)}));
        } else if off != Mapped::Unsupported {
            params.insert("reasoning".into(), json!({"effort": off_or_none()}));
        }
    } else if format == "ant-ling" && reasoning && effort.is_some() {
        if let Mapped::Value(mapped) = model.thinking_level(effort.unwrap_or_default()) {
            params.insert("reasoning".into(), json!({"effort": mapped}));
        }
    } else if format == "together" && reasoning {
        params.insert("reasoning".into(), json!({"enabled": effort.is_some()}));
        if let (Some(effort), true) = (effort, compat.supports_reasoning_effort) {
            params.insert("reasoning_effort".into(), json!(model.mapped_or(effort)));
        }
    } else if format == "string-thinking" && reasoning {
        if let Some(effort) = effort {
            params.insert("thinking".into(), json!(model.mapped_or(effort)));
        } else if off != Mapped::Unsupported {
            params.insert("thinking".into(), json!(off_or_none()));
        }
    } else if let (Some(effort), true, true) = (effort, reasoning, compat.supports_reasoning_effort) {
        params.insert("reasoning_effort".into(), json!(model.mapped_or(effort)));
    } else if effort.is_none()
        && reasoning
        && compat.supports_reasoning_effort
        && let Mapped::Value(off) = off
    {
        params.insert("reasoning_effort".into(), json!(off));
    }
    if let (Some(field), Some(budget)) = (budget_field, budget) {
        params.insert(field, json!(budget));
    }
    if let Some(routing) = model.compat_value("openRouterRouting") {
        params.insert("provider".into(), routing.clone());
    }
    if let Some(routing) = model.compat_value("vercelGatewayRouting") {
        let mut gateway = Map::new();
        for key in ["only", "order"] {
            if let Some(list) = routing.get(key).filter(|v| !v.is_null()) {
                gateway.insert(key.into(), list.clone());
            }
        }
        if !gateway.is_empty() {
            params.insert("providerOptions".into(), json!({"gateway": gateway}));
        }
    }
    if let Some(sampling) = &base.sampling_params {
        params.extend(sampling.clone());
    }
    Ok(params)
}

fn chunk_usage(raw: &Value, model: &Model) -> Usage {
    let number = |path: &str| raw.pointer(path).filter(|v| !v.is_null());
    let prompt = number("/prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as i64;
    let cache_read = number("/prompt_tokens_details/cached_tokens")
        .or_else(|| number("/prompt_cache_hit_tokens"))
        .or_else(|| number("/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = number("/prompt_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input = (prompt - cache_read as i64 - cache_write as i64).max(0) as u64;
    let output = number("/completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: Some(
            number("/completion_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
        total_tokens: input + output + cache_read + cache_write,
        ..Usage::default()
    };
    calculate_cost(model, &mut usage);
    usage
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        other => (StopReason::Error, Some(format!("Provider finish_reason: {other}"))),
    }
}

/// Per tool-call scratch state upstream keeps on the block.
#[derive(Default)]
struct ToolScratch {
    partial_args: Option<String>,
    custom: Option<(String, GrammarInputBuffer)>,
    stream_index: Option<i64>,
}

struct Run<'s, 'e> {
    output: AssistantMessage,
    sink: &'s mut EventSink<'e>,
    scratch: Vec<ToolScratch>,
    text_block: Option<usize>,
    thinking_block: Option<usize>,
    has_finish_reason: bool,
    by_index: HashMap<i64, usize>,
    by_id: HashMap<String, usize>,
    reasoning_details: Option<Vec<Value>>,
}

impl Run<'_, '_> {
    fn emit(&mut self, event: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        let event = event(self.output.clone());
        (self.sink)(&event);
    }

    fn push(&mut self, block: AssistantContent, scratch: ToolScratch) -> usize {
        self.output.content.push(block);
        self.scratch.push(scratch);
        self.output.content.len() - 1
    }

    fn call(&mut self, position: usize) -> &mut ToolCall {
        match &mut self.output.content[position] {
            AssistantContent::ToolCall(call) => call,
            _ => unreachable!("tool scratch always indexes a tool call"),
        }
    }

    fn custom_input(&mut self, position: usize) -> String {
        let Some((property, _)) = &self.scratch[position].custom else {
            return String::new();
        };
        let property = property.clone();
        self.call(position)
            .arguments
            .get(&property)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn append_custom(&mut self, position: usize, next: &str, close: bool) -> Result<Option<String>, String> {
        let Some((property, buffer)) = &mut self.scratch[position].custom else {
            return Ok(None);
        };
        let delta = buffer.append(property, next, close)?;
        let property = property.clone();
        self.call(position).arguments = json!({ property: next });
        Ok(delta)
    }

    fn apply_reasoning_details(&mut self, position: usize) {
        if let (Some(details), AssistantContent::Thinking(block)) =
            (&self.reasoning_details, &mut self.output.content[position])
        {
            block.thinking_signature = Some(stringify(&Value::Array(details.clone())));
        }
    }

    fn ensure_text(&mut self) -> usize {
        if let Some(position) = self.text_block {
            return position;
        }
        let position = self.push(
            AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            }),
            ToolScratch::default(),
        );
        self.text_block = Some(position);
        self.emit(|partial| AssistantMessageEvent::TextStart {
            content_index: position,
            partial,
        });
        position
    }

    fn ensure_thinking(&mut self, signature: &str) -> usize {
        if let Some(position) = self.thinking_block {
            return position;
        }
        let block = ThinkingContent {
            thinking: String::new(),
            thinking_signature: Some(signature.to_string()),
            redacted: None,
        };
        let position = self.push(AssistantContent::Thinking(block), ToolScratch::default());
        self.thinking_block = Some(position);
        self.emit(|partial| AssistantMessageEvent::ThinkingStart {
            content_index: position,
            partial,
        });
        position
    }

    fn ensure_tool_call(&mut self, delta: &Value, grammar: &IndexMap<String, String>) -> usize {
        let present = |key: &str| delta.get(key).filter(|v| !js_falsy(Some(v)));
        let stream_index = delta.get("index").and_then(Value::as_i64);
        let function = present("function");
        let custom = present("custom");
        let name = delta
            .pointer("/function/name")
            .filter(|v| !v.is_null())
            .or_else(|| delta.pointer("/custom/name").filter(|v| !v.is_null()))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = present("id").and_then(Value::as_str).map(str::to_string);
        let mut position = stream_index.and_then(|i| self.by_index.get(&i).copied());
        if position.is_none()
            && let Some(id) = &id
        {
            position = self.by_id.get(id).copied();
        }
        let position = match position {
            Some(position) => position,
            None => {
                let custom_property = (custom.is_some() && function.is_none())
                    .then(|| grammar.get(&name).cloned().unwrap_or_else(|| "input".into()));
                let call = ToolCall {
                    id: id.clone().unwrap_or_default(),
                    name: name.clone(),
                    arguments: match &custom_property {
                        Some(property) => json!({ property: "" }),
                        None => json!({}),
                    },
                    thought_signature: None,
                    namespace: None,
                };
                let scratch = ToolScratch {
                    partial_args: custom_property.is_none().then(String::new),
                    custom: custom_property.map(|p| (p, GrammarInputBuffer::default())),
                    stream_index,
                };
                let position = self.push(AssistantContent::ToolCall(call), scratch);
                if let Some(index) = stream_index {
                    self.by_index.insert(index, position);
                }
                if let Some(id) = &id {
                    self.by_id.insert(id.clone(), position);
                }
                self.emit(|partial| AssistantMessageEvent::ToolCallStart {
                    content_index: position,
                    partial,
                });
                position
            }
        };
        if let (Some(index), None) = (stream_index, self.scratch[position].stream_index) {
            self.scratch[position].stream_index = Some(index);
            self.by_index.insert(index, position);
        }
        if let Some(id) = &id {
            self.by_id.insert(id.clone(), position);
        }
        let call = self.call(position);
        if call.name.is_empty() && !name.is_empty() {
            call.name = name;
        }
        if custom.is_some() && function.is_none() && self.scratch[position].custom.is_none() {
            let call_name = self.call(position).name.clone();
            let property = grammar.get(&call_name).cloned().unwrap_or_else(|| "input".into());
            self.call(position).arguments = json!({ property.clone(): "" });
            self.scratch[position].custom = Some((property, GrammarInputBuffer::default()));
            self.scratch[position].partial_args = None;
        }
        position
    }

    fn handle_chunk(
        &mut self,
        chunk: &Value,
        model: &Model,
        grammar: &IndexMap<String, String>,
    ) -> Result<(), Failure> {
        if !chunk.is_object() {
            return Ok(());
        }
        if self.output.response_id.as_deref().is_none_or(str::is_empty) {
            self.output.response_id = chunk.get("id").and_then(Value::as_str).map(str::to_string);
        }
        if let Some(response_model) = chunk.get("model").and_then(Value::as_str)
            && !response_model.is_empty()
            && response_model != model.id
            && self.output.response_model.as_deref().is_none_or(str::is_empty)
        {
            self.output.response_model = Some(response_model.to_string());
        }
        let chunk_usage_present = chunk.get("usage").is_some_and(|u| !js_falsy(Some(u)));
        if chunk_usage_present {
            self.output.usage = chunk_usage(&chunk["usage"], model);
        }
        let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else {
            return Ok(());
        };
        if !chunk_usage_present && let Some(usage) = choice.get("usage").filter(|u| !js_falsy(Some(u))) {
            self.output.usage = chunk_usage(usage, model);
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            self.output.raw_stop_reason = Some(reason.to_string());
            let (stop, message) = map_stop_reason(reason);
            self.output.stop_reason = stop;
            if message.is_some() {
                self.output.error_message = message;
            }
            self.has_finish_reason = true;
        }
        let Some(delta) = choice.get("delta").filter(|d| !js_falsy(Some(d))) else {
            return Ok(());
        };
        if let Some(content) = delta.get("content").and_then(Value::as_str).filter(|c| !c.is_empty()) {
            let position = self.ensure_text();
            if let AssistantContent::Text(block) = &mut self.output.content[position] {
                block.text.push_str(content);
            }
            let delta = content.to_string();
            self.emit(|partial| AssistantMessageEvent::TextDelta {
                content_index: position,
                delta,
                partial,
            });
        }
        let found = ["reasoning_content", "reasoning", "reasoning_text"]
            .into_iter()
            .find_map(|field| {
                delta
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(|v| (field, v))
            });
        if let Some((field, text)) = found {
            let signature = if model.provider == "opencode-go" && field == "reasoning" {
                "reasoning_content"
            } else {
                field
            };
            let position = self.ensure_thinking(signature);
            if let AssistantContent::Thinking(block) = &mut self.output.content[position] {
                block.thinking.push_str(text);
            }
            let delta = text.to_string();
            self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                content_index: position,
                delta,
                partial,
            });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call_delta in calls {
                let position = self.ensure_tool_call(call_delta, grammar);
                let id = call_delta.get("id").and_then(Value::as_str).filter(|i| !i.is_empty());
                if let Some(id) = id
                    && self.call(position).id.is_empty()
                {
                    self.call(position).id = id.to_string();
                    self.by_id.insert(id.to_string(), position);
                }
                let name = call_delta
                    .pointer("/function/name")
                    .filter(|v| !v.is_null())
                    .or_else(|| call_delta.pointer("/custom/name"))
                    .and_then(Value::as_str);
                if let Some(name) = name.filter(|n| !n.is_empty())
                    && self.call(position).name.is_empty()
                {
                    self.call(position).name = name.to_string();
                }
                let mut text = String::new();
                if let Some(arguments) = call_delta
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .filter(|a| !a.is_empty())
                {
                    text = arguments.to_string();
                    let partial = self.scratch[position].partial_args.get_or_insert_with(String::new);
                    partial.push_str(arguments);
                    let parsed = parse_streaming_json(partial);
                    self.call(position).arguments = parsed;
                } else if let Some(input) = call_delta
                    .pointer("/custom/input")
                    .and_then(Value::as_str)
                    .filter(|i| !i.is_empty())
                {
                    let next = format!("{}{input}", self.custom_input(position));
                    text = self
                        .append_custom(position, &next, false)
                        .map_err(Failure::plain)?
                        .unwrap_or_default();
                }
                self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                    content_index: position,
                    delta: text,
                    partial,
                });
            }
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details.iter().filter(|d| is_reasoning_detail(d)) {
                self.ensure_thinking("");
                append_reasoning_detail(self.reasoning_details.get_or_insert_with(Vec::new), detail);
            }
        }
        Ok(())
    }

    fn finish_blocks(&mut self) -> Result<(), Failure> {
        for position in 0..self.output.content.len() {
            match &self.output.content[position] {
                AssistantContent::Text(block) => {
                    let content = block.text.clone();
                    self.emit(|partial| AssistantMessageEvent::TextEnd {
                        content_index: position,
                        content,
                        partial,
                    });
                }
                AssistantContent::Thinking(_) => {
                    self.apply_reasoning_details(position);
                    let AssistantContent::Thinking(block) = &self.output.content[position] else {
                        continue;
                    };
                    let content = block.thinking.clone();
                    self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                        content_index: position,
                        content,
                        partial,
                    });
                }
                AssistantContent::ToolCall(_) => {
                    if self.scratch[position].custom.is_some() {
                        let input = self.custom_input(position);
                        if let Some(delta) = self.append_custom(position, &input, true).map_err(Failure::plain)? {
                            self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                                content_index: position,
                                delta,
                                partial,
                            });
                        }
                    } else {
                        let parsed =
                            parse_streaming_json(self.scratch[position].partial_args.as_deref().unwrap_or_default());
                        self.call(position).arguments = parsed;
                    }
                    let tool_call = self.call(position).clone();
                    self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                        content_index: position,
                        tool_call,
                        partial,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Stream one Chat Completions response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICompletionsOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let compat = Compat::of(model);
    let context = resolve_transcript(context, compat.supports_mid_convo_system_messages);
    let now = now_ms();
    let mut run = Run {
        output: AssistantMessage::pending(model, now),
        sink,
        scratch: Vec::new(),
        text_block: None,
        thinking_block: None,
        has_finish_reason: false,
        by_index: HashMap::new(),
        by_id: HashMap::new(),
        reasoning_details: None,
    };
    let signal = options.base.signal();
    match drive(&mut run, model, &context, options, &compat, transport, now).await {
        Ok(()) => {
            let reason = run.output.stop_reason;
            run.emit(|message| AssistantMessageEvent::Done { reason, message });
        }
        Err(failure) => {
            for position in 0..run.output.content.len() {
                run.apply_reasoning_details(position);
            }
            run.output.stop_reason = if is_aborted(signal) {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            let mut message = format_provider_error(&failure, None);
            if let Some(raw) = failure
                .raw_metadata
                .as_ref()
                .filter(|raw| !message.contains(raw.as_str()))
            {
                message.push('\n');
                message.push_str(raw);
            }
            run.output.error_message = Some(message);
            let reason = run.output.stop_reason;
            run.emit(|error| AssistantMessageEvent::Error { reason, error });
        }
    }
    run.output
}

fn client_headers(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    api_key: &str,
    session_id: Option<&str>,
    compat: &Compat,
) -> HeaderBuilder {
    let mut headers = HeaderBuilder::default();
    headers
        .set("Accept", "application/json")
        .set("Authorization", format!("Bearer {api_key}"));
    headers
        .set("User-Agent", pi_user_agent())
        .openai_sdk_identity()
        .extend_set(model.headers.iter().flatten());
    if model.provider == "github-copilot" {
        for (name, value) in copilot_headers(&context.messages) {
            headers.set(&name, value);
        }
    }
    if let (Some(session_id), true) = (session_id, compat.send_session_affinity_headers) {
        if compat.session_affinity_format == "openrouter" {
            headers.set("x-session-id", session_id);
        } else {
            if compat.session_affinity_format == "openai" {
                headers.set("session_id", session_id);
            }
            headers
                .set("x-client-request-id", session_id)
                .set("x-session-affinity", session_id);
        }
    }
    headers.extend(options.headers.iter().flatten());
    headers.set("Content-Type", "application/json");
    headers
}

/// The OpenAI SDK stream: JSON per `data:` event until `[DONE]`; a body with `error` throws an API error. An abort ends
/// the stream quietly, as the SDK does.
pub(crate) async fn read_openai_stream(
    response: &mut crate::http::HttpResponse,
    signal: Option<&crate::http::AbortSignal>,
    mut handle: impl FnMut(&Value) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let headers = response.headers.clone();
    let mut decoder = SseDecoder::default();
    let mut done = false;
    let mut process = |events: Vec<ServerSentEvent>, done: &mut bool| -> Result<(), Failure> {
        for sse in events {
            if *done {
                continue;
            }
            if sse.data.starts_with("[DONE]") {
                *done = true;
                continue;
            }
            if sse.event.as_deref().is_some_and(|e| e.starts_with("thread.")) {
                continue;
            }
            let data = parse_json(&sse.data).map_err(|e| Failure::plain(e.to_string()))?;
            if let Some(error) = data.get("error").filter(|e| !js_falsy(Some(e))) {
                return Err(Failure::api(
                    Sdk::OpenAi,
                    None,
                    Some(headers.clone()),
                    Some(json!({ "error": error })),
                    None,
                ));
            }
            handle(&data)?;
        }
        Ok(())
    };
    loop {
        match read_chunk(response.body.as_mut(), signal).await {
            Read::Chunk(bytes) => {
                process(decoder.push(&bytes), &mut done)?;
                if is_aborted(signal) {
                    return Ok(());
                }
            }
            Read::End => break,
            Read::Aborted => return Ok(()),
            Read::Failed(error) => return Err(Failure::plain(error)),
        }
    }
    process(decoder.finish(false), &mut done)
}

async fn drive(
    run: &mut Run<'_, '_>,
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICompletionsOptions,
    compat: &Compat,
    transport: &dyn Transport,
    now: i64,
) -> Result<(), Failure> {
    let base = &options.base;
    let signal = base.signal();
    let api_key = client_api_key(model, base).map_err(Failure::plain)?;
    let grammar =
        grammar_tool_input_properties(&declared_tools(&context.messages), compat.supports_openai_grammar_tools)
            .map_err(Failure::plain)?;
    let retention = base.cache_retention();
    let session_id = if retention == CacheRetention::None {
        None
    } else {
        base.session_id.as_deref()
    };
    let headers = client_headers(model, context, base, &api_key, session_id, compat);
    let mut params =
        Value::Object(build_params(model, context, options, compat, retention, &grammar, now).map_err(Failure::plain)?);
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&params, model)) {
        params = next;
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: join_url(&model.base_url, "/chat/completions"),
        headers: headers.build(),
        body: serde_json::to_string(&params).unwrap_or_default(),
    };
    let mut response = send_with_retry(transport, &request, Sdk::OpenAi, base.retry(), signal).await?;
    run.emit(|partial| AssistantMessageEvent::Start { partial });
    read_openai_stream(&mut response, signal, |chunk| run.handle_chunk(chunk, model, &grammar)).await?;
    run.finish_blocks()?;
    if is_aborted(signal) || run.output.stop_reason == StopReason::Aborted {
        return Err(Failure::plain("Request was aborted"));
    }
    if !run.has_finish_reason && !compat.supports_finish_reason {
        let has_calls = run
            .output
            .content
            .iter()
            .any(|b| matches!(b, AssistantContent::ToolCall(_)));
        run.output.stop_reason = if has_calls {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
    }
    if run.output.stop_reason == StopReason::Error {
        let message = run.output.error_message.clone().filter(|m| !m.is_empty());
        return Err(Failure::plain(
            message.unwrap_or_else(|| "Provider returned an error stop reason".into()),
        ));
    }
    if (compat.supports_finish_reason && !run.has_finish_reason) || run.output.stop_reason == StopReason::Pending {
        return Err(Failure::plain("Stream ended without finish_reason"));
    }
    Ok(())
}

/// `streamSimple`: reasoning clamped to the model's supported levels and sent as `reasoningEffort`.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    client_api_key(model, &options.base)?;
    let base = build_base_options(model, context, options);
    let effort = options
        .reasoning
        .as_deref()
        .map(|r| clamp_thinking_level(model, r))
        .filter(|e| *e != "off");
    let completions = OpenAICompletionsOptions {
        base,
        tool_choice: options.tool_choice.clone().map(Value::String),
        reasoning_effort: effort.map(str::to_string),
        thinking_budgets: options.thinking_budgets,
    };
    Ok(stream(model, context, &completions, transport, sink).await)
}
