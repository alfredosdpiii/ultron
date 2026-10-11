//! Azure OpenAI Responses provider (`api/azure-openai-responses.ts`): the OpenAI Responses wire format sent through the
//! `AzureOpenAI` SDK client (`api-key` header, `?api-version=` query, deployment-name model ids, Azure base URL
//! normalization), with the shared Responses event processing and no service-tier pricing.

use crate::constrained::grammar_tool_input_properties;
use crate::env::provider_env_value;
use crate::http::{
    Failure, HeaderBuilder, HttpRequest, Sdk, Transport, format_provider_error, is_aborted, send_with_retry,
};
use crate::models::clamp_thinking_level;
use crate::openai_completions::read_openai_stream;
use crate::openai_responses::{
    ConvertOptions, ResponsesRun, TierPricing, ToolOptions, convert_responses_messages, convert_responses_tools,
};
use crate::options::{SimpleStreamOptions, StreamOptions, build_base_options, now_ms, pi_user_agent};
use crate::transcript::{declared_tools, resolve_transcript, resolve_transcript_tools};
use crate::types::{AssistantMessage, AssistantMessageEvent, EventSink, Mapped, Model, StopReason, TranscriptContext};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value, json};

const DEFAULT_AZURE_API_VERSION: &str = "v1";
const AZURE_TOOL_CALL_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode", "azure-openai-responses"];
const MIN_OUTPUT_TOKENS: u64 = 16;

/// `AzureOpenAIResponsesOptions`.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureOpenAIResponsesOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub tool_choice: Option<Value>,
    pub reasoning_summary: Option<String>,
    pub azure_api_version: Option<String>,
    pub azure_resource_name: Option<String>,
    pub azure_base_url: Option<String>,
    pub azure_deployment_name: Option<String>,
}

/// `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`: `model=deployment` pairs separated by commas.
fn parse_deployment_name_map(value: Option<&str>) -> Vec<(String, String)> {
    let mut map: Vec<(String, String)> = Vec::new();
    for entry in value.unwrap_or_default().split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.splitn(3, '=');
        let (Some(model_id), Some(deployment)) = (parts.next(), parts.next()) else {
            continue;
        };
        if model_id.is_empty() || deployment.is_empty() {
            continue;
        }
        let key = model_id.trim().to_string();
        map.retain(|(k, _)| *k != key);
        map.push((key, deployment.trim().to_string()));
    }
    map
}

/// The deployment the request names as its `model`.
pub fn resolve_deployment_name(model: &Model, options: &AzureOpenAIResponsesOptions) -> String {
    if let Some(name) = options.azure_deployment_name.as_ref().filter(|n| !n.is_empty()) {
        return name.clone();
    }
    let map = provider_env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", options.base.env.as_ref());
    parse_deployment_name_map(map.as_deref())
        .into_iter()
        .find(|(id, _)| *id == model.id)
        .map(|(_, deployment)| deployment)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| model.id.clone())
}

fn trim_trailing_slashes(text: &str) -> &str {
    text.trim_end_matches('/')
}

/// `normalizeAzureBaseUrl`: Azure hosts get `/openai/v1` as their base path.
pub fn normalize_azure_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = trim_trailing_slashes(base_url.trim());
    let mut url = reqwest::Url::parse(trimmed).map_err(|_| format!("Invalid Azure OpenAI base URL: {base_url}"))?;
    let host = url.host_str().unwrap_or_default().to_string();
    let azure = host.ends_with(".openai.azure.com")
        || host.ends_with(".cognitiveservices.azure.com")
        || host.ends_with(".ai.azure.com");
    let path = trim_trailing_slashes(url.path()).to_string();
    if azure && matches!(path.as_str(), "" | "/" | "/openai" | "/openai/v1/responses") {
        url.set_path("/openai/v1");
        url.set_query(None);
    }
    Ok(trim_trailing_slashes(url.as_str()).to_string())
}

/// `resolveAzureConfig`: `(baseUrl, apiVersion)`.
pub fn resolve_azure_config(model: &Model, options: &AzureOpenAIResponsesOptions) -> Result<(String, String), String> {
    let env = options.base.env.as_ref();
    let api_version = options
        .azure_api_version
        .clone()
        .filter(|v| !v.is_empty())
        .or_else(|| provider_env_value("AZURE_OPENAI_API_VERSION", env))
        .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.into());
    let base_url = options
        .azure_base_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .or_else(|| {
            provider_env_value("AZURE_OPENAI_BASE_URL", env)
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
        });
    let resource = options
        .azure_resource_name
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| provider_env_value("AZURE_OPENAI_RESOURCE_NAME", env));
    let resolved = base_url
        .or_else(|| resource.map(|r| format!("https://{r}.openai.azure.com/openai/v1")))
        .or_else(|| (!model.base_url.is_empty()).then(|| model.base_url.clone()))
        .ok_or_else(|| {
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl.".to_string()
        })?;
    Ok((normalize_azure_base_url(&resolved)?, api_version))
}

/// The SDK's URL: `{baseURL}/responses` plus the `api-version` default query.
fn request_url(base_url: &str, api_version: &str) -> Result<String, String> {
    let joined = crate::http::join_url(base_url, "/responses");
    let mut url = reqwest::Url::parse(&joined).map_err(|e| e.to_string())?;
    url.query_pairs_mut().append_pair("api-version", api_version);
    Ok(url.to_string())
}

fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: &AzureOpenAIResponsesOptions,
    deployment: &str,
    grammar: &IndexMap<String, String>,
    now: i64,
) -> Result<Map<String, Value>, String> {
    let flag = |key: &str, default: bool| model.compat_bool(key).unwrap_or(default);
    let supports_additional_tools = flag("supportsAdditionalTools", false);
    let supports_tool_search = flag("supportsToolSearch", false);
    let tool_options = ToolOptions {
        null_strict: false,
        supports_strict_mode: flag("supportsStrictMode", true),
        supports_openai_grammar_tools: flag("supportsOpenAIGrammarTools", false),
        tool_search_result: false,
    };
    let tools = resolve_transcript_tools(&context.messages, supports_additional_tools || supports_tool_search);
    let input = convert_responses_messages(
        model,
        context,
        &ConvertOptions {
            tool_call_providers: AZURE_TOOL_CALL_PROVIDERS,
            include_system_prompt: true,
            grammar,
            supports_mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            supports_additional_tools,
            supports_tool_search,
            tool_options,
        },
        now,
    )?;
    let base = &options.base;
    let mut params = Map::new();
    params.insert("model".into(), json!(deployment));
    params.insert("input".into(), Value::Array(input));
    params.insert("stream".into(), json!(true));
    if let Some(session) = &base.session_id {
        params.insert(
            "prompt_cache_key".into(),
            json!(crate::openai_prompt_cache::clamp_openai_prompt_cache_key(session)),
        );
    }
    params.insert("store".into(), json!(false));
    if let Some(max_tokens) = base.max_tokens.filter(|m| *m > 0) {
        params.insert("max_output_tokens".into(), json!(max_tokens.max(MIN_OUTPUT_TOKENS)));
    }
    if let Some(temperature) = base.temperature {
        params.insert("temperature".into(), json!(temperature));
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
        } else if model.thinking_level("off") != Mapped::Unsupported {
            let off = match model.thinking_level("off") {
                Mapped::Value(v) => v,
                _ => "none",
            };
            params.insert("reasoning".into(), json!({"effort": off}));
        }
    }
    if let Some(sampling) = &base.sampling_params {
        params.extend(sampling.clone());
    }
    Ok(params)
}

/// Stream one Azure OpenAI Responses response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &AzureOpenAIResponsesOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let context = resolve_transcript(
        context,
        model.compat_bool("supportsMidConvoSystemMessages").unwrap_or(false),
    );
    let now = now_ms();
    let mut output = AssistantMessage::pending(model, now);
    output.api = "azure-openai-responses".into();
    let mut run = ResponsesRun::new(output, sink);
    let signal = options.base.signal();
    match drive(&mut run, model, &context, options, transport, now).await {
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
            run.output.error_message = Some(format_provider_error(&failure, Some("Azure OpenAI API error")));
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
    options: &AzureOpenAIResponsesOptions,
    transport: &dyn Transport,
    now: i64,
) -> Result<(), Failure> {
    let deployment = resolve_deployment_name(model, options);
    let base = &options.base;
    let signal = base.signal();
    let api_key = base
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Failure::plain(format!("No API key for provider: {}", model.provider)))?;
    let (base_url, api_version) = resolve_azure_config(model, options).map_err(Failure::plain)?;
    let grammar = grammar_tool_input_properties(
        &declared_tools(&context.messages),
        model.compat_bool("supportsOpenAIGrammarTools").unwrap_or(false),
    )
    .map_err(Failure::plain)?;
    let mut params =
        Value::Object(build_params(model, context, options, &deployment, &grammar, now).map_err(Failure::plain)?);
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&params, model)) {
        params = next;
    }
    let mut headers = HeaderBuilder::default();
    headers.set("Accept", "application/json").set("api-key", api_key);
    headers
        .set("User-Agent", pi_user_agent())
        .extend_set(model.headers.iter().flatten());
    headers.extend(base.headers.iter().flatten());
    headers.set("Content-Type", "application/json");
    let request = HttpRequest {
        method: "POST".into(),
        url: request_url(&base_url, &api_version).map_err(Failure::plain)?,
        headers: headers.build(),
        body: serde_json::to_string(&params).unwrap_or_default(),
    };
    let mut response = send_with_retry(transport, &request, Sdk::OpenAi, base.retry(), signal).await?;
    run.emit(|partial| AssistantMessageEvent::Start { partial });
    read_openai_stream(&mut response, signal, |event| {
        run.handle(event, model, &grammar, TierPricing::None)
            .map_err(Failure::plain)
    })
    .await?;
    run.end().map_err(Failure::plain)?;
    if is_aborted(signal) {
        return Err(Failure::plain("Request was aborted"));
    }
    match run.output.stop_reason {
        StopReason::Pending => Err(Failure::plain(
            "Azure OpenAI Responses stream ended without a stop reason",
        )),
        StopReason::Aborted | StopReason::Error => Err(Failure::plain(
            run.output
                .error_message
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "An unknown error occurred".into()),
        )),
        _ => Ok(()),
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
    let azure = AzureOpenAIResponsesOptions {
        base,
        reasoning_effort: effort.map(str::to_string),
        tool_choice: options.tool_choice.clone().map(Value::String),
        ..Default::default()
    };
    Ok(stream(model, context, &azure, transport, sink).await)
}
