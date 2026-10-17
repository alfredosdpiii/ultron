//! Image generation: the built-in image model catalog (`image-models.ts`, shipped as `data/image-models.json` by
//! `data/generate-image-models.mjs`), API dispatch (`images.ts`, `images-api-registry.ts`), the `ImagesModels`
//! collection (`images-models.ts`) and the OpenRouter images API (`api/openrouter-images.ts`).
//!
//! Evidence: `fixtures/generate-images.mjs` drives the TypeScript `generateImages` and `builtinImagesModels()` through
//! a stubbed `fetch`; `tests/images.rs` replays the recorded responses here and compares requests and results.

use crate::env::ProviderEnv;
use crate::http::{
    AbortSignal, BoxFuture, Failure, HeaderBuilder, HttpRequest, Sdk, Transport, format_provider_error, is_aborted,
    join_url, send_with_retry,
};
use crate::json::parse_json;
use crate::options::ProviderHeaders;
use crate::types::{Cost, ImageContent, ModelCost, TextContent, Usage};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

/// `ImagesModel`: a model without the chat-only fields, plus its output modalities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesModel {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub input: Vec<String>,
    pub output: Vec<String>,
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// Model fields the images layer does not read (`inputLimits`, `promptCache`, ...), kept for round trips.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Text or image: image-generation input and output content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ImagesContent {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<ImagesContent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImagesStopReason {
    Stop,
    Error,
    Aborted,
}

/// `AssistantImages`: the result of one generation; failures are carried in `stop_reason`/`error_message`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub output: Vec<ImagesContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: ImagesStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

impl AssistantImages {
    fn empty(model: &ImagesModel, now: i64) -> Self {
        AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            stop_reason: ImagesStopReason::Stop,
            error_message: None,
            timestamp: now,
        }
    }

    fn error(model: &ImagesModel, message: String, now: i64) -> Self {
        AssistantImages {
            stop_reason: ImagesStopReason::Error,
            error_message: Some(message),
            ..Self::empty(model, now)
        }
    }
}

/// Inspect or replace the images payload before it is sent; `None` keeps it.
pub type ImagesPayloadHook = Arc<dyn Fn(&Value, &ImagesModel) -> Option<Value> + Send + Sync>;

/// `ImagesOptions`.
#[derive(Clone, Default)]
pub struct ImagesOptions {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub env: Option<ProviderEnv>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<f64>,
    /// `timeoutMs`: how long each attempt may wait for the response headers (the SDK's request `timeout`).
    pub timeout_ms: Option<u64>,
    pub metadata: Option<Map<String, Value>>,
    pub signal: Option<AbortSignal>,
    pub on_payload: Option<ImagesPayloadHook>,
    /// `Date.now()` for the result timestamp; `None` reads the clock.
    pub now_ms: Option<i64>,
}

impl ImagesOptions {
    fn now(&self) -> i64 {
        self.now_ms.unwrap_or_else(crate::options::now_ms)
    }
}

// ---------------------------------------------------------------------------------------------- catalog

type Catalog = IndexMap<String, IndexMap<String, ImagesModel>>;

static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../data/image-models.json")).expect("built-in image model catalog parses")
});

/// `getImageProviders`: built-in image providers in catalog order.
pub fn image_providers() -> impl Iterator<Item = &'static str> {
    CATALOG.keys().map(String::as_str)
}

/// `getImageModels`: built-in image models of `provider`; empty for an unknown provider.
pub fn image_models(provider: &str) -> impl Iterator<Item = &'static ImagesModel> {
    CATALOG.get(provider).into_iter().flat_map(IndexMap::values)
}

/// `getImageModel`.
pub fn image_model(provider: &str, id: &str) -> Option<&'static ImagesModel> {
    CATALOG.get(provider)?.get(id)
}

// ---------------------------------------------------------------------------------------------- API dispatch

/// An image-generation API implementation (`ProviderImages`).
pub trait ImagesApi: Send + Sync {
    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        context: &'a ImagesContext,
        options: &'a ImagesOptions,
        transport: &'a dyn Transport,
    ) -> BoxFuture<'a, AssistantImages>;
}

/// The OpenRouter images API (`openrouter-images`).
pub struct OpenRouterImages;

impl ImagesApi for OpenRouterImages {
    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        context: &'a ImagesContext,
        options: &'a ImagesOptions,
        transport: &'a dyn Transport,
    ) -> BoxFuture<'a, AssistantImages> {
        Box::pin(openrouter_generate_images(model, context, options, transport))
    }
}

/// A registered API: `generate_images` rejects a model of another api (`wrapGenerateImages`).
#[derive(Clone)]
pub struct RegisteredImagesApi {
    api: String,
    implementation: Arc<dyn ImagesApi>,
}

impl RegisteredImagesApi {
    pub async fn generate_images(
        &self,
        model: &ImagesModel,
        context: &ImagesContext,
        options: &ImagesOptions,
        transport: &dyn Transport,
    ) -> Result<AssistantImages, String> {
        if model.api != self.api {
            return Err(format!("Mismatched api: {} expected {}", model.api, self.api));
        }
        Ok(self
            .implementation
            .generate_images(model, context, options, transport)
            .await)
    }
}

static REGISTRY: LazyLock<RwLock<HashMap<String, RegisteredImagesApi>>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "openrouter-images".to_string(),
        RegisteredImagesApi {
            api: "openrouter-images".into(),
            implementation: Arc::new(OpenRouterImages),
        },
    );
    RwLock::new(map)
});

/// `registerImagesApiProvider`: registers (or replaces) the implementation for `api`.
pub fn register_images_api_provider(api: &str, implementation: Arc<dyn ImagesApi>) {
    REGISTRY.write().unwrap_or_else(|e| e.into_inner()).insert(
        api.to_string(),
        RegisteredImagesApi {
            api: api.to_string(),
            implementation,
        },
    );
}

/// `getImagesApiProvider`.
pub fn images_api_provider(api: &str) -> Option<RegisteredImagesApi> {
    REGISTRY.read().unwrap_or_else(|e| e.into_inner()).get(api).cloned()
}

/// `generateImages` of `images.ts`: dispatch by `model.api`. `Err` when no implementation is registered.
pub async fn generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    transport: &dyn Transport,
) -> Result<AssistantImages, String> {
    let provider =
        images_api_provider(&model.api).ok_or_else(|| format!("No API provider registered for api: {}", model.api))?;
    provider.generate_images(model, context, options, transport).await
}

// ---------------------------------------------------------------------------------------------- OpenRouter images

/// The pinned OpenAI SDK's `User-Agent`; upstream sends it unchanged for images requests.
pub const OPENAI_SDK_USER_AGENT: &str = "OpenAI/JS 6.40.0";

/// `buildParams`: one user message with the input parts; `modalities` include text when the model outputs text.
pub fn openrouter_images_params(model: &ImagesModel, context: &ImagesContext) -> Value {
    let content: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            // sanitizeSurrogates is a no-op: a Rust string cannot hold a lone surrogate.
            ImagesContent::Text(text) => json!({"type": "text", "text": text.text}),
            ImagesContent::Image(image) => json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{}", image.mime_type, image.data)},
            }),
        })
        .collect();
    let modalities = if model.output.iter().any(|o| o == "text") {
        json!(["image", "text"])
    } else {
        json!(["image"])
    };
    json!({
        "model": model.id,
        "messages": [{"role": "user", "content": content}],
        "stream": false,
        "modalities": modalities,
    })
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).filter(|n| *n != 0.0).unwrap_or(0.0)
}

/// `parseUsage`: cache writes are reported inside `cached_tokens`; input excludes both cache kinds.
fn openrouter_usage(raw: &Value, model: &ImagesModel) -> Usage {
    let prompt = number(raw.get("prompt_tokens"));
    let reported_cached = number(raw.pointer("/prompt_tokens_details/cached_tokens"));
    let cache_write = number(raw.pointer("/prompt_tokens_details/cache_write_tokens"));
    let cache_read = if cache_write > 0.0 {
        (reported_cached - cache_write).max(0.0)
    } else {
        reported_cached
    };
    let input = (prompt - cache_read - cache_write).max(0.0);
    let output = number(raw.get("completion_tokens"));
    let mut cost = Cost {
        input: (model.cost.input / 1_000_000.0) * input,
        output: (model.cost.output / 1_000_000.0) * output,
        cache_read: (model.cost.cache_read / 1_000_000.0) * cache_read,
        cache_write: (model.cost.cache_write / 1_000_000.0) * cache_write,
        total: 0.0,
    };
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    Usage {
        input: input as u64,
        output: output as u64,
        cache_read: cache_read as u64,
        cache_write: cache_write as u64,
        total_tokens: (input + output + cache_read + cache_write) as u64,
        cost,
        ..Usage::default()
    }
}

/// `data:<mime>;base64,<data>` (`/^data:([^;]+);base64,(.+)$/`).
fn parse_data_url(url: &str) -> Option<ImageContent> {
    let rest = url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    if mime.is_empty() || mime.contains(';') || data.is_empty() || data.contains(['\n', '\r']) {
        return None;
    }
    Some(ImageContent {
        data: data.to_string(),
        mime_type: mime.to_string(),
    })
}

fn openrouter_output(response: &Value, out: &mut AssistantImages, model: &ImagesModel) {
    out.response_id = response.get("id").and_then(Value::as_str).map(str::to_owned);
    if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
        out.usage = Some(openrouter_usage(usage, model));
    }
    let Some(message) = response.pointer("/choices/0/message") else {
        return;
    };
    if let Some(text) = message.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        out.output.push(ImagesContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        }));
    }
    for image in message.get("images").and_then(Value::as_array).into_iter().flatten() {
        let url = match image.get("image_url") {
            Some(Value::String(url)) => Some(url.as_str()),
            Some(other) => other.get("url").and_then(Value::as_str),
            None => None,
        };
        if let Some(image) = url.and_then(parse_data_url) {
            out.output.push(ImagesContent::Image(image));
        }
    }
}

async fn openrouter_request(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    transport: &dyn Transport,
    out: &mut AssistantImages,
) -> Result<(), Failure> {
    let api_key = options
        .api_key
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Failure::plain(format!("No API key for provider: {}", model.provider)))?;
    // `{ ...model.headers, ...options.headers }` (case-sensitive spread), nulls dropped, then the SDK's defaults.
    let mut custom: IndexMap<String, Option<String>> = model
        .headers
        .iter()
        .flatten()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect();
    custom.extend(options.headers.iter().flatten().map(|(k, v)| (k.clone(), v.clone())));
    let mut headers = HeaderBuilder::default();
    headers
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .set("User-Agent", OPENAI_SDK_USER_AGENT)
        .openai_sdk_identity()
        .set("Authorization", format!("Bearer {api_key}"));
    for (name, value) in &custom {
        if let Some(value) = value {
            headers.set(name, value.clone());
        }
    }
    let mut params = openrouter_images_params(model, context);
    if let Some(next) = options.on_payload.as_ref().and_then(|hook| hook(&params, model)) {
        params = next;
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: join_url(&model.base_url, "/chat/completions"),
        headers: headers.build(),
        body: serde_json::to_string(&params).unwrap_or_default(),
    };
    let retry = crate::http::RetryOptions {
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        ..Default::default()
    };
    let signal = options.signal.as_ref();
    let timed;
    let transport = match options.timeout_ms {
        Some(ms) => {
            timed = crate::http::TimeoutTransport {
                inner: transport,
                timeout: std::time::Duration::from_millis(ms),
            };
            &timed as &dyn Transport
        }
        None => transport,
    };
    let mut response = send_with_retry(transport, &request, Sdk::OpenAi, retry, signal).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.body.next_chunk().await.map_err(Failure::plain)? {
        bytes.extend(chunk);
    }
    let body = parse_json(&String::from_utf8_lossy(&bytes)).map_err(|e| Failure::plain(e.to_string()))?;
    openrouter_output(&body, out, model);
    Ok(())
}

/// `generateImages` of `api/openrouter-images.ts`. Never fails: errors are returned in the result.
pub async fn openrouter_generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    transport: &dyn Transport,
) -> AssistantImages {
    let mut out = AssistantImages::empty(model, options.now());
    if let Err(failure) = openrouter_request(model, context, options, transport, &mut out).await {
        out.stop_reason = if is_aborted(options.signal.as_ref()) {
            ImagesStopReason::Aborted
        } else {
            ImagesStopReason::Error
        };
        out.error_message = Some(format_provider_error(&failure, None));
    }
    out
}

// ---------------------------------------------------------------------------------------------- auth seam

/// Request auth resolved for an images provider (`AuthResult` flattened).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImagesAuthResult {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub base_url: Option<String>,
    pub env: Option<ProviderEnv>,
    pub source: Option<String>,
}

/// `AuthResolutionOverrides` as `ImagesModels.generateImages` passes them.
#[derive(Clone, Debug, Default)]
pub struct ImagesAuthOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
}

/// Provider auth: `Ok(None)` when unconfigured, `Err` (the `ModelsError` message) on a real failure.
pub trait ImagesAuth: Send + Sync {
    fn resolve<'a>(
        &'a self,
        provider_id: &'a str,
        overrides: &'a ImagesAuthOverrides,
    ) -> BoxFuture<'a, Result<Option<ImagesAuthResult>, String>>;
}

/// Environment lookup of an `AuthContext` (`ctx.env(name)`).
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// `envApiKeyAuth` under `resolveProviderAuth` with no stored credential: an explicit key wins (with the request
/// env), else the first set variable, read from the request env before `lookup`.
pub struct EnvApiKeyAuth {
    pub env_vars: Vec<String>,
    pub lookup: EnvLookup,
}

impl ImagesAuth for EnvApiKeyAuth {
    fn resolve<'a>(
        &'a self,
        _provider_id: &'a str,
        overrides: &'a ImagesAuthOverrides,
    ) -> BoxFuture<'a, Result<Option<ImagesAuthResult>, String>> {
        Box::pin(async move {
            if let Some(key) = overrides.api_key.as_deref().filter(|k| !k.is_empty()) {
                return Ok(Some(ImagesAuthResult {
                    api_key: Some(key.to_string()),
                    env: overrides.env.clone(),
                    source: Some("stored credential".into()),
                    ..Default::default()
                }));
            }
            for name in &self.env_vars {
                let value = overrides
                    .env
                    .as_ref()
                    .and_then(|env| env.get(name))
                    .filter(|v| !v.is_empty())
                    .cloned()
                    .or_else(|| (self.lookup)(name).filter(|v| !v.is_empty()));
                if let Some(value) = value {
                    return Ok(Some(ImagesAuthResult {
                        api_key: Some(value),
                        source: Some(name.clone()),
                        ..Default::default()
                    }));
                }
            }
            Ok(None)
        })
    }
}

// ---------------------------------------------------------------------------------------------- ImagesModels

/// Dynamic model listing (`refreshModels`): `Err` keeps the last-known list.
pub type RefreshImagesModels = Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<ImagesModel>, String>> + Send + Sync>;

/// `createImagesProvider`: an image-generation provider built from parts.
pub struct ImagesProvider {
    pub id: String,
    pub name: String,
    pub auth: Arc<dyn ImagesAuth>,
    pub api: Arc<dyn ImagesApi>,
    models: Mutex<Vec<ImagesModel>>,
    refresh_models: Option<RefreshImagesModels>,
    refresh_lock: tokio::sync::Mutex<Result<(), String>>,
    refreshes: AtomicU64,
}

impl ImagesProvider {
    pub fn new(
        id: &str,
        name: Option<&str>,
        auth: Arc<dyn ImagesAuth>,
        models: Vec<ImagesModel>,
        refresh_models: Option<RefreshImagesModels>,
        api: Arc<dyn ImagesApi>,
    ) -> Self {
        ImagesProvider {
            id: id.to_string(),
            name: name.unwrap_or(id).to_string(),
            auth,
            api,
            models: Mutex::new(models),
            refresh_models,
            refresh_lock: tokio::sync::Mutex::new(Ok(())),
            refreshes: AtomicU64::new(0),
        }
    }

    /// Current known models (the list as of the last successful refresh for dynamic providers).
    pub fn models(&self) -> Vec<ImagesModel> {
        self.models.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_dynamic(&self) -> bool {
        self.refresh_models.is_some()
    }

    /// `refreshModels`: concurrent callers share one in-flight fetch and its outcome. No-op for static providers.
    pub async fn refresh_models(&self) -> Result<(), String> {
        let Some(fetch) = &self.refresh_models else {
            return Ok(());
        };
        let seen = self.refreshes.load(Ordering::SeqCst);
        let mut last = self.refresh_lock.lock().await;
        if self.refreshes.load(Ordering::SeqCst) != seen {
            return last.clone();
        }
        let result = fetch().await.map(|models| {
            *self.models.lock().unwrap_or_else(|e| e.into_inner()) = models;
        });
        *last = result.clone();
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        result
    }
}

/// `ModelsError` raised by the images collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagesModelsError {
    /// `"model_source"`, `"provider"`, `"auth"` or `"oauth"`.
    pub code: &'static str,
    pub message: String,
}

/// `createImagesModels`: the runtime collection of image providers.
#[derive(Default)]
pub struct ImagesModels {
    providers: IndexMap<String, Arc<ImagesProvider>>,
}

impl ImagesModels {
    pub fn new() -> Self {
        Self::default()
    }

    /// Upsert by id; a replaced provider keeps its position.
    pub fn set_provider(&mut self, provider: ImagesProvider) {
        self.providers.insert(provider.id.clone(), Arc::new(provider));
    }

    pub fn delete_provider(&mut self, id: &str) {
        self.providers.shift_remove(id);
    }

    pub fn clear_providers(&mut self) {
        self.providers.clear();
    }

    pub fn providers(&self) -> Vec<Arc<ImagesProvider>> {
        self.providers.values().cloned().collect()
    }

    pub fn provider(&self, id: &str) -> Option<Arc<ImagesProvider>> {
        self.providers.get(id).cloned()
    }

    /// Last-known models of one provider, or of all providers in registration order.
    pub fn models(&self, provider: Option<&str>) -> Vec<ImagesModel> {
        match provider {
            Some(id) => self.providers.get(id).map(|p| p.models()).unwrap_or_default(),
            None => self.providers.values().flat_map(|p| p.models()).collect(),
        }
    }

    pub fn model(&self, provider: &str, id: &str) -> Option<ImagesModel> {
        self.models(Some(provider)).into_iter().find(|m| m.id == id)
    }

    /// With a provider id, a fetch failure is `model_source`; without one, all providers refresh best-effort.
    pub async fn refresh(&self, provider: Option<&str>) -> Result<(), ImagesModelsError> {
        match provider {
            Some(id) => {
                let Some(entry) = self.providers.get(id) else {
                    return Ok(());
                };
                entry.refresh_models().await.map_err(|cause| ImagesModelsError {
                    code: "model_source",
                    message: with_cause(&format!("Model refresh failed for {id}"), &cause),
                })
            }
            None => {
                for entry in self.providers.values() {
                    let _ = entry.refresh_models().await;
                }
                Ok(())
            }
        }
    }

    /// `getAuth`: `Ok(None)` for an unknown or unconfigured provider.
    pub async fn auth(
        &self,
        provider_id: &str,
        overrides: &ImagesAuthOverrides,
    ) -> Result<Option<ImagesAuthResult>, String> {
        match self.providers.get(provider_id) {
            Some(provider) => provider.auth.resolve(provider_id, overrides).await,
            None => Ok(None),
        }
    }

    /// Generate through the owning provider with auth merged in; explicit options win per field, headers and env
    /// merge per key. Never fails: errors come back as a result with `stop_reason` `error`.
    pub async fn generate_images(
        &self,
        model: &ImagesModel,
        context: &ImagesContext,
        options: &ImagesOptions,
        transport: &dyn Transport,
    ) -> AssistantImages {
        let now = options.now();
        let Some(provider) = self.providers.get(&model.provider) else {
            return AssistantImages::error(model, format!("Unknown provider: {}", model.provider), now);
        };
        let overrides = ImagesAuthOverrides {
            api_key: options.api_key.clone(),
            env: options.env.clone(),
        };
        let resolution = match provider.auth.resolve(&provider.id, &overrides).await {
            Ok(resolution) => resolution,
            Err(message) => return AssistantImages::error(model, message, now),
        };
        let Some(auth) = resolution else {
            return provider.api.generate_images(model, context, options, transport).await;
        };
        let request_model = match &auth.base_url {
            Some(base_url) => ImagesModel {
                base_url: base_url.clone(),
                ..model.clone()
            },
            None => model.clone(),
        };
        let headers = if auth.headers.is_some() || options.headers.is_some() {
            let mut merged = auth.headers.clone().unwrap_or_default();
            merged.extend(options.headers.clone().unwrap_or_default());
            Some(merged)
        } else {
            None
        };
        let env = if auth.env.is_some() || options.env.is_some() {
            let mut merged = auth.env.clone().unwrap_or_default();
            merged.extend(options.env.clone().unwrap_or_default());
            Some(merged)
        } else {
            None
        };
        let request_options = ImagesOptions {
            api_key: options.api_key.clone().or(auth.api_key),
            headers,
            env,
            ..options.clone()
        };
        provider
            .api
            .generate_images(&request_model, context, &request_options, transport)
            .await
    }
}

/// `withCauseDetail`: the cause is appended unless the message already contains it.
fn with_cause(message: &str, cause: &str) -> String {
    let detail = cause.trim();
    if detail.is_empty() || message.contains(detail) {
        message.to_string()
    } else {
        format!("{message}: {detail}")
    }
}

/// `openrouterImagesProvider`: OpenRouter with its built-in image catalog and `OPENROUTER_API_KEY` auth.
/// ponytail: upstream also offers OpenRouter OAuth here; stored credentials are wired with the auth module.
pub fn openrouter_images_provider(lookup: EnvLookup) -> ImagesProvider {
    ImagesProvider::new(
        "openrouter",
        Some("OpenRouter"),
        Arc::new(EnvApiKeyAuth {
            env_vars: vec!["OPENROUTER_API_KEY".into()],
            lookup,
        }),
        image_models("openrouter").cloned().collect(),
        None,
        Arc::new(OpenRouterImages),
    )
}

/// `builtinImagesModels`.
pub fn builtin_images_models(lookup: EnvLookup) -> ImagesModels {
    let mut models = ImagesModels::new();
    models.set_provider(openrouter_images_provider(lookup));
    models
}
