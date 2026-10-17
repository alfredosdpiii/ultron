//! OpenAI Responses provider (`api/openai-responses.ts` with `api/openai-responses-shared.ts`): request construction,
//! response-event processing, usage with service-tier pricing, and stop reasons.

use crate::anthropic::copilot_headers;
use crate::constrained::{
    GrammarInputBuffer, grammar_tool_input, grammar_tool_input_properties, json_schema_tool_parameters,
    resolve_grammar_constrained_sampling, resolve_json_schema_strict_sampling,
};
use crate::http::{
    Failure, HeaderBuilder, HttpRequest, Sdk, Transport, format_provider_error, has_header, is_aborted, join_url,
    send_with_retry,
};
use crate::json::{js_len, parse_json, parse_streaming_json, short_hash, stringify};
use crate::models::{calculate_cost, clamp_thinking_level};
use crate::openai_completions::read_openai_stream;
use crate::options::{SimpleStreamOptions, StreamOptions, build_base_options, now_ms, pi_user_agent};
use crate::transcript::{
    declared_tools, render_system_message_update, resolve_transcript, resolve_transcript_tools, system_message_text,
    transform_messages,
};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, EventSink, Mapped, Message, Model,
    StopReason, SystemMessage, TextContent, ThinkingContent, Tool, ToolCall, TranscriptContext, Usage, UserContent,
    UserMessageContent,
};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

pub(crate) const OPENAI_TOOL_CALL_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode"];
const MIN_OUTPUT_TOKENS: u64 = 16;

/// `OpenAIResponsesOptions`.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAIResponsesOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub reasoning_effort: Option<String>,
    /// `"auto" | "detailed" | "concise"`.
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub tool_choice: Option<Value>,
}

struct Compat {
    supports_mid_convo_system_messages: bool,
    session_affinity_format: String,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
    supports_openai_grammar_tools: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
    supports_explicit_prompt_cache_mode: bool,
    supports_max_output_tokens: bool,
}

impl Compat {
    fn of(model: &Model) -> Self {
        let flag = |key: &str, default: bool| model.compat_bool(key).unwrap_or(default);
        let openrouter = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
        Compat {
            supports_mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            session_affinity_format: model
                .compat_str("sessionAffinityFormat")
                .unwrap_or(if openrouter { "openrouter" } else { "openai" })
                .to_string(),
            supports_long_cache_retention: flag("supportsLongCacheRetention", true),
            supports_strict_mode: flag("supportsStrictMode", false),
            supports_openai_grammar_tools: flag("supportsOpenAIGrammarTools", false),
            supports_additional_tools: flag("supportsAdditionalTools", false),
            supports_tool_search: flag("supportsToolSearch", false),
            supports_explicit_prompt_cache_mode: flag("supportsExplicitPromptCacheMode", false),
            supports_max_output_tokens: flag("supportsMaxOutputTokens", true),
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

/// `String(value)` for an optional JSON field (`undefined` when absent).
fn js_str(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => stringify(other),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Options for converting tools (`ConvertResponsesToolsOptions`).
#[derive(Clone, Copy, Default)]
pub struct ToolOptions {
    /// `strict: null` (Codex): tools without a constrained-sampling choice send `"strict": null`, not `false`.
    pub null_strict: bool,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub tool_search_result: bool,
}

pub fn convert_responses_tools(tools: &[Tool], options: ToolOptions) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        let mut converted = Map::new();
        if let Some(grammar) = resolve_grammar_constrained_sampling(tool, options.supports_openai_grammar_tools)? {
            converted.insert("type".into(), json!("custom"));
            converted.insert("name".into(), json!(tool.name));
            converted.insert("description".into(), json!(tool.description));
            converted.insert(
                "format".into(),
                json!({"type": "grammar", "syntax": grammar.format, "definition": grammar.definition}),
            );
            if options.tool_search_result {
                converted.insert("defer_loading".into(), json!(true));
            }
            out.push(Value::Object(converted));
            continue;
        }
        let constrained = resolve_json_schema_strict_sampling(tool, options.supports_strict_mode)?;
        let strict = constrained.or(if options.null_strict { None } else { Some(false) });
        converted.insert("type".into(), json!("function"));
        converted.insert("name".into(), json!(tool.name));
        converted.insert("description".into(), json!(tool.description));
        converted.insert(
            "parameters".into(),
            json_schema_tool_parameters(tool, Some(strict == Some(true)))?,
        );
        if options.tool_search_result {
            converted.insert("defer_loading".into(), json!(true));
        }
        if options.supports_strict_mode {
            converted.insert("strict".into(), json!(strict));
        }
        out.push(Value::Object(converted));
    }
    Ok(out)
}

fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature.filter(|s| !s.is_empty())?;
    if signature.starts_with('{')
        && let Ok(parsed) = parse_json(signature)
        && parsed.get("v").and_then(Value::as_i64) == Some(1)
        && let Some(id) = str_field(&parsed, "id")
    {
        let phase = str_field(&parsed, "phase")
            .filter(|p| *p == "commentary" || *p == "final_answer")
            .map(str::to_string);
        return Some((id.to_string(), phase));
    }
    Some((signature.to_string(), None))
}

fn input_image(data: &str, mime_type: &str) -> Value {
    json!({"type": "input_image", "detail": "auto", "image_url": format!("data:{mime_type};base64,{data}")})
}

fn tool_result_output(model: &Model, content: &[UserContent]) -> Value {
    let text = content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<Value> = content
        .iter()
        .filter_map(|c| match c {
            UserContent::Image(i) => Some(input_image(&i.data, &i.mime_type)),
            UserContent::Text(_) => None,
        })
        .collect();
    if images.is_empty() || !model.supports_images() {
        return json!(if !text.is_empty() {
            text
        } else if !images.is_empty() {
            "(see attached image)".into()
        } else {
            "(no tool output)".to_string()
        });
    }
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(json!({"type": "input_text", "text": text}));
    }
    output.extend(images);
    Value::Array(output)
}

fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = sanitize(part).chars().take(64).collect();
    sanitized.trim_end_matches('_').to_string()
}

pub struct ConvertOptions<'a> {
    /// Providers whose `call|item` tool call ids keep their item part (`allowedToolCallProviders`).
    pub tool_call_providers: &'a [&'a str],
    pub include_system_prompt: bool,
    pub grammar: &'a IndexMap<String, String>,
    pub supports_mid_convo_system_messages: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub tool_options: ToolOptions,
}

/// `convertResponsesMessages`: the transcript as Responses input items.
pub fn convert_responses_messages(
    model: &Model,
    context: &TranscriptContext,
    options: &ConvertOptions<'_>,
    now: i64,
) -> Result<Vec<Value>, String> {
    let context = resolve_transcript(context, options.supports_mid_convo_system_messages);
    let allowed = options.tool_call_providers.contains(&model.provider.as_str());
    let normalize = |id: &str, source: &AssistantMessage| -> String {
        if !allowed || !id.contains('|') {
            return normalize_id_part(id);
        }
        let mut parts = id.split('|');
        let call_id = normalize_id_part(parts.next().unwrap_or_default());
        let item_id = parts.next().unwrap_or_default();
        let foreign = source.provider != model.provider || source.api != model.api;
        let mut normalized_item = if foreign {
            let hashed = format!("fc_{}", short_hash(item_id));
            hashed.chars().take(64).collect()
        } else {
            normalize_id_part(item_id)
        };
        if !normalized_item.starts_with("fc_") {
            normalized_item = normalize_id_part(&format!("fc_{normalized_item}"));
        }
        format!("{call_id}|{normalized_item}")
    };
    let transformed = transform_messages(&context.messages, model, Some(&normalize), now);
    let tools = resolve_transcript_tools(
        &context.messages,
        options.supports_additional_tools || options.supports_tool_search,
    );
    let mut messages: Vec<Value> = Vec::new();
    let append_tool_additions = |messages: &mut Vec<Value>,
                                 message: &SystemMessage,
                                 seed: String|
     -> Result<(), String> {
        let added = if tools.anchors_additions {
            message.tools_added.clone().unwrap_or_default()
        } else {
            vec![]
        };
        if added.is_empty() {
            return Ok(());
        }
        if options.supports_additional_tools {
            messages.push(json!({"type": "additional_tools", "role": "developer", "tools": convert_responses_tools(&added, options.tool_options)?}));
            return Ok(());
        }
        if !options.supports_tool_search {
            return Ok(());
        }
        let names: Vec<&str> = added.iter().map(|t| t.name.as_str()).collect();
        let call_id = format!("pi_tool_load_{}", short_hash(&format!("{seed}:{}", names.join(","))));
        messages.push(json!({
            "type": "tool_search_call",
            "call_id": call_id,
            "execution": "client",
            "status": "completed",
            "arguments": {"query": names.join(" "), "limit": names.len()},
        }));
        let search_options = ToolOptions {
            tool_search_result: true,
            ..options.tool_options
        };
        messages.push(json!({
            "type": "tool_search_output",
            "call_id": call_id,
            "execution": "client",
            "status": "completed",
            "tools": convert_responses_tools(&added, search_options)?,
        }));
        Ok(())
    };
    let instruction_role = if model.reasoning && model.compat_bool("supportsDeveloperRole") != Some(false) {
        "developer"
    } else {
        "system"
    };
    let mut msg_index = 0;
    for (source_index, message) in transformed.iter().enumerate() {
        let leading = source_index == 0 && matches!(message, Message::System(_));
        match message {
            Message::System(system) => {
                if !leading {
                    append_tool_additions(&mut messages, system, format!("system:{msg_index}"))?;
                }
                if !leading || options.include_system_prompt {
                    let text = if leading {
                        system_message_text(system)
                    } else {
                        render_system_message_update(system)
                    };
                    if !text.is_empty() {
                        messages.push(json!({"role": instruction_role, "content": text}));
                    }
                }
            }
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => {
                    messages.push(json!({"role": "user", "content": [{"type": "input_text", "text": text}]}))
                }
                UserMessageContent::Blocks(blocks) => {
                    if blocks.is_empty() {
                        continue;
                    }
                    let content: Vec<Value> = blocks
                        .iter()
                        .map(|b| match b {
                            UserContent::Text(t) => json!({"type": "input_text", "text": t.text}),
                            UserContent::Image(i) => input_image(&i.data, &i.mime_type),
                        })
                        .collect();
                    messages.push(json!({"role": "user", "content": content}));
                }
            },
            Message::Assistant(assistant) => {
                let mut output = Vec::new();
                let same_provider = assistant.provider == model.provider && assistant.api == model.api;
                let same_model = same_provider && assistant.model == model.id;
                let different_model = same_provider && assistant.model != model.id;
                let mut text_index = 0;
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if let Some(signature) = thinking.thinking_signature.as_deref().filter(|s| !s.is_empty()) {
                                output.push(parse_json(signature).map_err(|e| e.to_string())?);
                            }
                        }
                        AssistantContent::Text(text) => {
                            let parsed = parse_text_signature(text.text_signature.as_deref());
                            let fallback = if text_index == 0 {
                                format!("msg_pi_{msg_index}")
                            } else {
                                format!("msg_pi_{msg_index}_{text_index}")
                            };
                            text_index += 1;
                            let id = match parsed.as_ref().map(|(id, _)| id.as_str()).filter(|id| !id.is_empty()) {
                                None => fallback,
                                Some(id) if js_len(id) > 64 => format!("msg_{}", short_hash(id)),
                                Some(id) => id.to_string(),
                            };
                            let mut item = Map::new();
                            item.insert("type".into(), json!("message"));
                            item.insert("role".into(), json!("assistant"));
                            item.insert(
                                "content".into(),
                                json!([{"type": "output_text", "text": text.text, "annotations": []}]),
                            );
                            item.insert("status".into(), json!("completed"));
                            item.insert("id".into(), json!(id));
                            if let Some(phase) = parsed.and_then(|(_, phase)| phase) {
                                item.insert("phase".into(), json!(phase));
                            }
                            output.push(Value::Object(item));
                        }
                        AssistantContent::ToolCall(call) => {
                            let mut parts = call.id.split('|');
                            let call_id = parts.next().unwrap_or_default();
                            let mut item_id = parts.next();
                            let custom = options.grammar.get(&call.name);
                            let fc = item_id.is_some_and(|i| i.starts_with("fc_"));
                            if (different_model && fc) || (custom.is_none() && !fc) {
                                item_id = None;
                            }
                            let mut item = Map::new();
                            item.insert(
                                "type".into(),
                                json!(if custom.is_some() {
                                    "custom_tool_call"
                                } else {
                                    "function_call"
                                }),
                            );
                            if let Some(item_id) = item_id {
                                item.insert("id".into(), json!(item_id));
                            }
                            item.insert("call_id".into(), json!(call_id));
                            item.insert("name".into(), json!(call.name));
                            match custom {
                                Some(property) => item.insert(
                                    "input".into(),
                                    json!(grammar_tool_input(&call.name, &call.arguments, property)?),
                                ),
                                None => item.insert("arguments".into(), json!(stringify(&call.arguments))),
                            };
                            if let (true, Some(namespace)) = (same_model, &call.namespace) {
                                item.insert("namespace".into(), json!(namespace));
                            }
                            output.push(Value::Object(item));
                        }
                    }
                }
                if output.is_empty() {
                    continue;
                }
                messages.extend(output);
            }
            Message::ToolResult(result) => {
                let call_id = result.tool_call_id.split('|').next().unwrap_or_default();
                let kind = if options.grammar.contains_key(&result.tool_name) {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                messages.push(
                    json!({"type": kind, "call_id": call_id, "output": tool_result_output(model, &result.content)}),
                );
            }
        }
        if !leading {
            msg_index += 1;
        }
    }
    Ok(messages)
}

fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAIResponsesOptions,
    compat: &Compat,
    grammar: &IndexMap<String, String>,
    now: i64,
) -> Result<Map<String, Value>, String> {
    let base = &options.base;
    let tools = resolve_transcript_tools(
        &context.messages,
        compat.supports_additional_tools || compat.supports_tool_search,
    );
    let tool_options = ToolOptions {
        null_strict: false,
        supports_strict_mode: compat.supports_strict_mode,
        supports_openai_grammar_tools: compat.supports_openai_grammar_tools,
        tool_search_result: false,
    };
    let input = convert_responses_messages(
        model,
        context,
        &ConvertOptions {
            tool_call_providers: OPENAI_TOOL_CALL_PROVIDERS,
            include_system_prompt: true,
            grammar,
            supports_mid_convo_system_messages: compat.supports_mid_convo_system_messages,
            supports_additional_tools: compat.supports_additional_tools,
            supports_tool_search: compat.supports_tool_search,
            tool_options,
        },
        now,
    )?;
    let retention = base.cache_retention();
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("input".into(), Value::Array(input));
    params.insert("stream".into(), json!(true));
    if retention != CacheRetention::None
        && let Some(session) = &base.session_id
    {
        params.insert(
            "prompt_cache_key".into(),
            json!(session.chars().take(64).collect::<String>()),
        );
    }
    let long = retention == CacheRetention::Long && compat.supports_long_cache_retention;
    if long && !compat.supports_explicit_prompt_cache_mode {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if compat.supports_explicit_prompt_cache_mode {
        if retention == CacheRetention::None {
            params.insert("prompt_cache_options".into(), json!({"mode": "explicit"}));
        } else if long {
            params.insert("prompt_cache_options".into(), json!({"ttl": "30m"}));
        }
    }
    params.insert("store".into(), json!(false));
    if let Some(max_tokens) = base.max_tokens.filter(|m| *m > 0)
        && compat.supports_max_output_tokens
    {
        params.insert("max_output_tokens".into(), json!(max_tokens.max(MIN_OUTPUT_TOKENS)));
    }
    if let Some(temperature) = base.temperature {
        params.insert("temperature".into(), json!(temperature));
    }
    if let Some(tier) = &options.service_tier {
        params.insert("service_tier".into(), json!(tier));
    }
    if !tools.request_tools.is_empty() {
        params.insert(
            "tools".into(),
            Value::Array(convert_responses_tools(&tools.request_tools, tool_options)?),
        );
    }
    if let Some(choice) = &options.tool_choice {
        params.insert("tool_choice".into(), choice.clone());
    }
    if model.reasoning {
        let effort = options.reasoning_effort.as_deref().filter(|e| !e.is_empty());
        let summary = options.reasoning_summary.as_deref().filter(|s| !s.is_empty());
        if effort.is_some() || summary.is_some() {
            let effort = effort.map_or("medium", |e| model.mapped_or(e));
            params.insert(
                "reasoning".into(),
                json!({"effort": effort, "summary": summary.unwrap_or("auto")}),
            );
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else if model.provider != "github-copilot" && model.thinking_level("off") != Mapped::Unsupported {
            let off = match model.thinking_level("off") {
                Mapped::Value(v) => v,
                _ => "none",
            };
            params.insert("reasoning".into(), json!({"effort": off}));
        }
        if model.provider == "xai" {
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }
    if let Some(sampling) = &base.sampling_params {
        params.extend(sampling.clone());
    }
    Ok(params)
}

/// How `processResponsesStream` prices a service tier (`resolveServiceTier`/`applyServiceTierPricing`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum TierPricing<'a> {
    /// No tier pricing (Azure).
    None,
    /// The response's tier, else the requested one (OpenAI Responses).
    Response(Option<&'a str>),
    /// Codex: a `default` response tier keeps a requested `flex`/`priority`.
    Codex(Option<&'a str>),
}

fn service_tier_multiplier(model: &Model, tier: Option<&str>) -> f64 {
    match tier {
        Some("flex") => 0.5,
        Some("priority") if model.id == "gpt-5.5" => 2.5,
        Some("priority") => 2.0,
        _ => 1.0,
    }
}

fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> Result<(StopReason, Option<String>), String> {
    let Some(status) = status else {
        return Ok((StopReason::Stop, None));
    };
    Ok(match status {
        "completed" | "in_progress" | "queued" => (StopReason::Stop, None),
        "incomplete" if incomplete_reason == Some("max_output_tokens") => (StopReason::Length, None),
        "incomplete" => (
            StopReason::Error,
            Some(match incomplete_reason {
                Some(reason) => format!("Response incomplete: {reason}"),
                None => "Response incomplete without a provider reason".into(),
            }),
        ),
        "failed" | "cancelled" => (StopReason::Error, None),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Thinking,
    Text,
    ToolCall,
}

#[derive(Default)]
struct ToolScratch {
    partial_json: Option<String>,
    custom: Option<(String, GrammarInputBuffer)>,
}

/// `processResponsesStream` state.
pub(crate) struct ResponsesRun<'s, 'e> {
    pub output: AssistantMessage,
    sink: &'s mut EventSink<'e>,
    slots: HashMap<i64, (SlotKind, usize)>,
    scratch: HashMap<usize, ToolScratch>,
    reasoning_by_id: HashMap<String, usize>,
    saw_terminal: bool,
}

impl<'s, 'e> ResponsesRun<'s, 'e> {
    pub fn new(output: AssistantMessage, sink: &'s mut EventSink<'e>) -> Self {
        ResponsesRun {
            output,
            sink,
            slots: HashMap::new(),
            scratch: HashMap::new(),
            reasoning_by_id: HashMap::new(),
            saw_terminal: false,
        }
    }

    pub(crate) fn emit(&mut self, event: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        let event = event(self.output.clone());
        (self.sink)(&event);
    }

    fn slot(&self, output_index: Option<i64>, kind: SlotKind) -> Option<usize> {
        let (slot_kind, position) = *self.slots.get(&output_index?)?;
        (slot_kind == kind).then_some(position)
    }

    fn call(&mut self, position: usize) -> &mut ToolCall {
        match &mut self.output.content[position] {
            AssistantContent::ToolCall(call) => call,
            _ => unreachable!("tool slots index tool calls"),
        }
    }

    fn apply_phase(&mut self, item: &Value) {
        if str_field(item, "type") == Some("message") && str_field(item, "phase") == Some("final_answer") {
            self.output.stop_reason = StopReason::Stop;
        }
    }

    fn create_slot(
        &mut self,
        output_index: Option<i64>,
        item: &Value,
        grammar: &IndexMap<String, String>,
    ) -> Option<(SlotKind, usize)> {
        let namespace = item.get("namespace").filter(|n| !n.is_null()).map(|n| js_str(Some(n)));
        let (kind, block, scratch) = match str_field(item, "type") {
            Some("reasoning") => (
                SlotKind::Thinking,
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                }),
                None,
            ),
            Some("message") => {
                self.apply_phase(item);
                (
                    SlotKind::Text,
                    AssistantContent::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }),
                    None,
                )
            }
            Some("function_call") => (
                SlotKind::ToolCall,
                AssistantContent::ToolCall(ToolCall {
                    id: format!("{}|{}", js_str(item.get("call_id")), js_str(item.get("id"))),
                    name: js_str(item.get("name")),
                    arguments: json!({}),
                    thought_signature: None,
                    namespace,
                }),
                Some(ToolScratch {
                    partial_json: Some(str_field(item, "arguments").unwrap_or_default().to_string()),
                    custom: None,
                }),
            ),
            Some("custom_tool_call") => {
                let name = js_str(item.get("name"));
                let property = grammar.get(&name).cloned().unwrap_or_else(|| "input".into());
                let input = str_field(item, "input").unwrap_or_default();
                (
                    SlotKind::ToolCall,
                    AssistantContent::ToolCall(ToolCall {
                        id: format!("{}|{}", js_str(item.get("call_id")), js_str(item.get("id"))),
                        name,
                        arguments: json!({ property.clone(): input }),
                        thought_signature: None,
                        namespace,
                    }),
                    Some(ToolScratch {
                        partial_json: None,
                        custom: Some((property, GrammarInputBuffer::default())),
                    }),
                )
            }
            _ => return None,
        };
        self.output.content.push(block);
        let position = self.output.content.len() - 1;
        if let Some(scratch) = scratch {
            self.scratch.insert(position, scratch);
        }
        if let Some(index) = output_index {
            self.slots.insert(index, (kind, position));
        }
        self.emit(|partial| match kind {
            SlotKind::Thinking => AssistantMessageEvent::ThinkingStart {
                content_index: position,
                partial,
            },
            SlotKind::Text => AssistantMessageEvent::TextStart {
                content_index: position,
                partial,
            },
            SlotKind::ToolCall => AssistantMessageEvent::ToolCallStart {
                content_index: position,
                partial,
            },
        });
        Some((kind, position))
    }

    fn custom_input(&mut self, position: usize) -> String {
        let Some((property, _)) = self.scratch.get(&position).and_then(|s| s.custom.as_ref()) else {
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
        let Some((property, buffer)) = self.scratch.get_mut(&position).and_then(|s| s.custom.as_mut()) else {
            return Ok(None);
        };
        let delta = buffer.append(property, next, close)?;
        let property = property.clone();
        self.call(position).arguments = json!({ property: next });
        Ok(delta)
    }

    fn push_tool_delta(&mut self, position: usize, delta: Option<String>) {
        if let Some(delta) = delta {
            self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                content_index: position,
                delta,
                partial,
            });
        }
    }

    fn has_partial_json(&self, position: usize) -> bool {
        self.scratch.get(&position).is_some_and(|s| s.partial_json.is_some())
    }

    fn has_custom(&self, position: usize) -> bool {
        self.scratch.get(&position).is_some_and(|s| s.custom.is_some())
    }

    fn append_text(&mut self, position: usize, delta: &str) {
        match &mut self.output.content[position] {
            AssistantContent::Text(block) => block.text.push_str(delta),
            AssistantContent::Thinking(block) => block.thinking.push_str(delta),
            AssistantContent::ToolCall(_) => {}
        }
    }

    fn finalize(&mut self, response: &Value, model: &Model, pricing: TierPricing<'_>) -> Result<(), String> {
        self.saw_terminal = true;
        for item in response.get("output").and_then(Value::as_array).into_iter().flatten() {
            let Some(encrypted) = item
                .get("encrypted_content")
                .filter(|e| e.as_str().is_some_and(|s| !s.is_empty()))
            else {
                continue;
            };
            if str_field(item, "type") != Some("reasoning") {
                continue;
            }
            let Some(&position) = self.reasoning_by_id.get(&js_str(item.get("id"))) else {
                continue;
            };
            let AssistantContent::Thinking(block) = &mut self.output.content[position] else {
                continue;
            };
            let Some(signature) = block.thinking_signature.as_deref().filter(|s| !s.is_empty()) else {
                continue;
            };
            let mut stored = parse_json(signature).map_err(|e| e.to_string())?;
            if stored
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                continue;
            }
            stored["encrypted_content"] = encrypted.clone();
            block.thinking_signature = Some(stringify(&stored));
        }
        if let Some(id) = str_field(response, "id").filter(|i| !i.is_empty()) {
            self.output.response_id = Some(id.to_string());
        }
        if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
            let count = |path: &str| usage.pointer(path).and_then(Value::as_u64).unwrap_or(0);
            let cached = count("/input_tokens_details/cached_tokens");
            let write = count("/input_tokens_details/cache_write_tokens");
            self.output.usage = Usage {
                input: (count("/input_tokens") as i64 - cached as i64 - write as i64).max(0) as u64,
                output: count("/output_tokens"),
                cache_read: cached,
                cache_write: write,
                reasoning: Some(count("/output_tokens_details/reasoning_tokens")),
                total_tokens: count("/total_tokens"),
                ..Usage::default()
            };
        }
        calculate_cost(model, &mut self.output.usage);
        let response_tier = str_field(response, "service_tier");
        let multiplier = match pricing {
            TierPricing::None => 1.0,
            TierPricing::Response(request) => service_tier_multiplier(model, response_tier.or(request)),
            TierPricing::Codex(request) => {
                let tier = match (response_tier, request) {
                    (Some("default"), Some(request @ ("flex" | "priority"))) => Some(request),
                    (response, request) => response.or(request),
                };
                service_tier_multiplier(model, tier)
            }
        };
        if multiplier != 1.0 {
            let cost = &mut self.output.usage.cost;
            cost.input *= multiplier;
            cost.output *= multiplier;
            cost.cache_read *= multiplier;
            cost.cache_write *= multiplier;
            cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
        }
        let status = str_field(response, "status");
        let reason = response.pointer("/incomplete_details/reason").and_then(Value::as_str);
        self.output.raw_stop_reason = match (status, reason) {
            (status, Some(reason)) => Some(format!("{}.{reason}", status.unwrap_or("undefined"))),
            (status, None) => status.map(str::to_string),
        };
        let (stop, message) = map_stop_reason(status, reason)?;
        self.output.stop_reason = stop;
        self.output.error_message = message;
        if stop == StopReason::Stop
            && self
                .output
                .content
                .iter()
                .any(|b| matches!(b, AssistantContent::ToolCall(_)))
        {
            self.output.stop_reason = StopReason::ToolUse;
        }
        Ok(())
    }

    /// One Responses stream event.
    pub fn handle(
        &mut self,
        event: &Value,
        model: &Model,
        grammar: &IndexMap<String, String>,
        pricing: TierPricing<'_>,
    ) -> Result<(), String> {
        let output_index = event.get("output_index").and_then(Value::as_i64);
        let delta = || str_field(event, "delta").unwrap_or_default().to_string();
        match str_field(event, "type").unwrap_or_default() {
            "response.created" => {
                self.output.response_id = event
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }
            "response.output_item.added" => {
                self.create_slot(output_index, &event["item"], grammar);
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_part.done" => {
                let Some(position) = self.slot(output_index, SlotKind::Thinking) else {
                    return Ok(());
                };
                let text = if str_field(event, "type") == Some("response.reasoning_summary_part.done") {
                    "\n\n".to_string()
                } else {
                    delta()
                };
                self.append_text(position, &text);
                self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                    content_index: position,
                    delta: text,
                    partial,
                });
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let Some(position) = self.slot(output_index, SlotKind::Text) else {
                    return Ok(());
                };
                let text = delta();
                self.append_text(position, &text);
                self.emit(|partial| AssistantMessageEvent::TextDelta {
                    content_index: position,
                    delta: text,
                    partial,
                });
            }
            "response.function_call_arguments.delta" => {
                let Some(position) = self
                    .slot(output_index, SlotKind::ToolCall)
                    .filter(|p| self.has_partial_json(*p))
                else {
                    return Ok(());
                };
                let text = delta();
                let partial = self
                    .scratch
                    .get_mut(&position)
                    .and_then(|s| s.partial_json.as_mut())
                    .expect("checked above");
                partial.push_str(&text);
                let parsed = parse_streaming_json(partial);
                self.call(position).arguments = parsed;
                self.push_tool_delta(position, Some(text));
            }
            "response.function_call_arguments.done" => {
                let Some(position) = self
                    .slot(output_index, SlotKind::ToolCall)
                    .filter(|p| self.has_partial_json(*p))
                else {
                    return Ok(());
                };
                let arguments = str_field(event, "arguments").unwrap_or_default().to_string();
                let slot = self
                    .scratch
                    .get_mut(&position)
                    .and_then(|s| s.partial_json.as_mut())
                    .expect("checked above");
                let previous = std::mem::replace(slot, arguments.clone());
                self.call(position).arguments = parse_streaming_json(&arguments);
                if let Some(rest) = arguments.strip_prefix(previous.as_str()).filter(|r| !r.is_empty()) {
                    self.push_tool_delta(position, Some(rest.to_string()));
                }
            }
            "response.custom_tool_call_input.delta" => {
                let Some(position) = self
                    .slot(output_index, SlotKind::ToolCall)
                    .filter(|p| self.has_custom(*p))
                else {
                    return Ok(());
                };
                let next = format!("{}{}", self.custom_input(position), delta());
                let appended = self.append_custom(position, &next, false)?;
                self.push_tool_delta(position, appended);
            }
            "response.custom_tool_call_input.done" => {
                let Some(position) = self
                    .slot(output_index, SlotKind::ToolCall)
                    .filter(|p| self.has_custom(*p))
                else {
                    return Ok(());
                };
                let input = str_field(event, "input").unwrap_or_default().to_string();
                let appended = self.append_custom(position, &input, true)?;
                self.push_tool_delta(position, appended);
            }
            "response.output_item.done" => {
                let item = &event["item"];
                self.apply_phase(item);
                let slot = match output_index.and_then(|i| self.slots.get(&i).copied()) {
                    Some(slot) => Some(slot),
                    None => self.create_slot(output_index, item, grammar),
                };
                let Some((kind, position)) = slot else { return Ok(()) };
                let item_type = str_field(item, "type").unwrap_or_default();
                let texts = |key: &str, sep: &str| -> String {
                    item.get(key)
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .map(|p| js_str(p.get("text")))
                                .collect::<Vec<_>>()
                                .join(sep)
                        })
                        .unwrap_or_default()
                };
                match (item_type, kind) {
                    ("reasoning", SlotKind::Thinking) => {
                        let summary = texts("summary", "\n\n");
                        let content = texts("content", "\n\n");
                        if let AssistantContent::Thinking(block) = &mut self.output.content[position] {
                            if !summary.is_empty() {
                                block.thinking = summary;
                            } else if !content.is_empty() {
                                block.thinking = content;
                            }
                            block.thinking_signature = Some(stringify(item));
                        }
                        self.reasoning_by_id.insert(js_str(item.get("id")), position);
                        let AssistantContent::Thinking(block) = &self.output.content[position] else {
                            return Ok(());
                        };
                        let content = block.thinking.clone();
                        self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                            content_index: position,
                            content,
                            partial,
                        });
                    }
                    ("message", SlotKind::Text) => {
                        let text = item
                            .get("content")
                            .and_then(Value::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .map(|p| {
                                        if str_field(p, "type") == Some("output_text") {
                                            js_str(p.get("text"))
                                        } else {
                                            js_str(p.get("refusal"))
                                        }
                                    })
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        let mut signature = Map::new();
                        signature.insert("v".into(), json!(1));
                        if let Some(id) = item.get("id").filter(|i| !i.is_null()) {
                            signature.insert("id".into(), id.clone());
                        }
                        if let Some(phase) = str_field(item, "phase").filter(|p| !p.is_empty()) {
                            signature.insert("phase".into(), json!(phase));
                        }
                        if let AssistantContent::Text(block) = &mut self.output.content[position] {
                            block.text = text.clone();
                            block.text_signature = Some(stringify(&Value::Object(signature)));
                        }
                        self.emit(|partial| AssistantMessageEvent::TextEnd {
                            content_index: position,
                            content: text,
                            partial,
                        });
                    }
                    ("function_call", SlotKind::ToolCall) if self.has_partial_json(position) => {
                        let partial = self
                            .scratch
                            .remove(&position)
                            .and_then(|s| s.partial_json)
                            .unwrap_or_default();
                        let source = str_field(item, "arguments")
                            .filter(|a| !a.is_empty())
                            .map(str::to_string);
                        let source = source.unwrap_or(if partial.is_empty() { "{}".into() } else { partial });
                        let call = self.call(position);
                        call.arguments = parse_streaming_json(&source);
                        if let Some(namespace) = item.get("namespace").filter(|n| !n.is_null()) {
                            call.namespace = Some(js_str(Some(namespace)));
                        }
                        let tool_call = call.clone();
                        self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                            content_index: position,
                            tool_call,
                            partial,
                        });
                    }
                    ("custom_tool_call", SlotKind::ToolCall) if self.has_custom(position) => {
                        let input = match item.get("input").filter(|i| !i.is_null()) {
                            Some(input) => js_str(Some(input)),
                            None => self.custom_input(position),
                        };
                        let appended = self.append_custom(position, &input, true)?;
                        self.push_tool_delta(position, appended);
                        self.scratch.remove(&position);
                        let call = self.call(position);
                        if let Some(namespace) = item.get("namespace").filter(|n| !n.is_null()) {
                            call.namespace = Some(js_str(Some(namespace)));
                        }
                        let tool_call = call.clone();
                        self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                            content_index: position,
                            tool_call,
                            partial,
                        });
                    }
                    _ => return Ok(()),
                }
                if let Some(index) = output_index {
                    self.slots.remove(&index);
                }
            }
            "response.completed" | "response.incomplete" => self.finalize(&event["response"], model, pricing)?,
            "error" => {
                return Err(format!(
                    "Error Code {}: {}",
                    js_str(event.get("code")),
                    js_str(event.get("message"))
                ));
            }
            "response.failed" => {
                self.saw_terminal = true;
                let response = &event["response"];
                self.output.raw_stop_reason = str_field(response, "status").map(str::to_string);
                let error = response.get("error").filter(|e| !e.is_null());
                let reason = response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .filter(|r| !r.is_empty());
                let message = match (error, reason) {
                    (Some(error), _) => {
                        let field = |key: &str, fallback: &str| {
                            error
                                .get(key)
                                .filter(|v| v.as_str().is_none_or(|s| !s.is_empty()) && !v.is_null())
                                .map_or(fallback.to_string(), |v| js_str(Some(v)))
                        };
                        format!("{}: {}", field("code", "unknown"), field("message", "no message"))
                    }
                    (None, Some(reason)) => format!("incomplete: {reason}"),
                    (None, None) => "Unknown error (no error details in response)".into(),
                };
                return Err(message);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn end(&self) -> Result<(), String> {
        if self.saw_terminal {
            Ok(())
        } else {
            Err("OpenAI Responses stream ended before a terminal response event".into())
        }
    }
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
    if let Some(session_id) = session_id {
        if compat.session_affinity_format == "openrouter" {
            headers.set("x-session-id", session_id);
        } else {
            if compat.session_affinity_format == "openai" {
                headers.set("session_id", session_id);
            }
            headers.set("x-client-request-id", session_id);
        }
    }
    headers.extend(options.headers.iter().flatten());
    headers.set("Content-Type", "application/json");
    headers
}

/// Stream one Responses API response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAIResponsesOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let compat = Compat::of(model);
    let context = resolve_transcript(context, compat.supports_mid_convo_system_messages);
    let now = now_ms();
    let mut run = ResponsesRun::new(AssistantMessage::pending(model, now), sink);
    let signal = options.base.signal();
    match drive(&mut run, model, &context, options, &compat, transport, now).await {
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
            let prefix = format!(
                "{} API error",
                if model.provider == "openai" {
                    "OpenAI"
                } else {
                    model.provider.as_str()
                }
            );
            run.output.error_message = Some(format_provider_error(&failure, Some(&prefix)));
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
    options: &OpenAIResponsesOptions,
    compat: &Compat,
    transport: &dyn Transport,
    now: i64,
) -> Result<(), Failure> {
    let base = &options.base;
    let signal = base.signal();
    let api_key = client_api_key(model, base).map_err(Failure::plain)?;
    let session_id = if base.cache_retention() == CacheRetention::None {
        None
    } else {
        base.session_id.as_deref()
    };
    let grammar =
        grammar_tool_input_properties(&declared_tools(&context.messages), compat.supports_openai_grammar_tools)
            .map_err(Failure::plain)?;
    let headers = client_headers(model, context, base, &api_key, session_id, compat);
    let mut params =
        Value::Object(build_params(model, context, options, compat, &grammar, now).map_err(Failure::plain)?);
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&params, model)) {
        params = next;
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: join_url(&model.base_url, "/responses"),
        headers: headers.build(),
        body: serde_json::to_string(&params).unwrap_or_default(),
    };
    let mut response = send_with_retry(transport, &request, Sdk::OpenAi, base.retry(), signal).await?;
    run.emit(|partial| AssistantMessageEvent::Start { partial });
    let tier = options.service_tier.as_deref();
    read_openai_stream(&mut response, signal, |event| {
        run.handle(event, model, &grammar, TierPricing::Response(tier))
            .map_err(Failure::plain)
    })
    .await?;
    run.end().map_err(Failure::plain)?;
    if is_aborted(signal) {
        return Err(Failure::plain("Request was aborted"));
    }
    match run.output.stop_reason {
        StopReason::Pending => Err(Failure::plain("OpenAI Responses stream ended without a stop reason")),
        StopReason::Aborted | StopReason::Error => {
            let message = run.output.error_message.clone().filter(|m| !m.is_empty());
            Err(Failure::plain(
                message.unwrap_or_else(|| "An unknown error occurred".into()),
            ))
        }
        _ => Ok(()),
    }
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
    let responses = OpenAIResponsesOptions {
        base,
        reasoning_effort: effort.map(str::to_string),
        tool_choice: options.tool_choice.clone().map(Value::String),
        ..Default::default()
    };
    Ok(stream(model, context, &responses, transport, sink).await)
}
