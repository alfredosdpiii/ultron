//! The user's `models.json` providers and models over the built-in catalog (`coding-agent/src/core/provider-composer.ts`
//! as `model-runtime.ts` uses it): custom providers and models, built-in providers with a new base URL, headers,
//! `compat` or models, per-model `modelOverrides`, composition errors (`getError`), availability (`checkAuth`) and
//! request auth (`getAuth`: the configured API key as a literal, `$VAR` template or `!command`, provider and model
//! headers, `authHeader`), plus the config values of `resolve-config-value.ts`.
//!
//! Credential-blind: callers pass the environment lookup and the key the built-in provider would use.
//!
//! Evidence: `fixtures/generate-model-registry.mjs` runs the TypeScript model runtime over models.json files;
//! `tests/model_registry.rs` replays it.

use crate::auth::merge_headers;
use crate::model_config::ModelConfig;
use crate::options::{ProviderHeaders, SimpleStreamOptions};
use crate::types::Model;
use config::auth::{TemplatePart, parse_template};
use serde_json::{Map, Value, json};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

/// An environment variable lookup.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// An environment variable lookup a model keeps for its requests.
pub type SharedLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What a stored OAuth credential adds to a request (`ModelAuth` beyond its key): the base URL it names (GitHub
/// Copilot's token proxy endpoint) and its headers (Kimi For Coding's `Authorization: Bearer <access>`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoredRequestAuth {
    pub base_url: Option<String>,
    pub headers: Option<ProviderHeaders>,
}

/// The [`StoredRequestAuth`] of the provider's stored OAuth credential, resolved (and refreshed) at request time.
pub type StoredAuth = Arc<dyn Fn() -> Option<StoredRequestAuth> + Send + Sync>;

/// What models.json adds to a model's requests, kept raw and resolved for each request as the TypeScript request
/// auth does (`getAuth` per request: `$VAR` templates read the environment then, `!command` values run then, with no
/// cache). A header that fails to resolve fails that request with its message.
#[derive(Clone, Default)]
pub struct RequestConfig {
    provider: String,
    model: String,
    /// `providers.<p>.headers`.
    provider_headers: Vec<(String, String)>,
    /// `modelOverrides.<id>.headers` under `models[].headers`.
    model_headers: Vec<(String, String)>,
    /// `authHeader`: send `Authorization: Bearer <key>`.
    pub auth_header: bool,
    lookup: Option<SharedLookup>,
    /// The provider's stored OAuth credential's request base URL and headers, when it has one.
    pub stored_auth: Option<StoredAuth>,
}

impl std::fmt::Debug for RequestConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestConfig")
            .field(
                "provider_headers",
                &self.provider_headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field(
                "model_headers",
                &self.model_headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("auth_header", &self.auth_header)
            .field("stored_auth", &self.stored_auth.is_some())
            .finish()
    }
}

impl PartialEq for RequestConfig {
    fn eq(&self, other: &Self) -> bool {
        (
            &self.provider,
            &self.model,
            &self.provider_headers,
            &self.model_headers,
            self.auth_header,
        ) == (
            &other.provider,
            &other.model,
            &other.provider_headers,
            &other.model_headers,
            other.auth_header,
        )
    }
}

impl RequestConfig {
    /// The request headers for `key` (`withConfiguredAuth`, then the model's headers), resolved now: provider
    /// headers, the bearer header, then model headers (names compare case-insensitively).
    pub fn headers(&self, provider: &str, key: Option<&str>) -> Result<Option<ProviderHeaders>, String> {
        match &self.lookup {
            Some(lookup) => self.headers_with(provider, key, &**lookup),
            None => self.headers_with(provider, key, &|_: &str| None),
        }
    }

    fn headers_with(
        &self,
        provider: &str,
        key: Option<&str>,
        lookup: Lookup<'_>,
    ) -> Result<Option<ProviderHeaders>, String> {
        let mut headers = resolve_headers_or_err(
            &self.provider_headers,
            &format!("provider \"{}\"", self.provider),
            lookup,
        )
        .map_err(|e| auth_failed(provider, &e))?;
        if self.auth_header {
            let key = key
                .filter(|k| !k.is_empty())
                .ok_or_else(|| auth_failed(provider, "authHeader requires a resolved API key"))?;
            headers
                .get_or_insert_default()
                .insert("Authorization".into(), Some(format!("Bearer {key}")));
        }
        let model = resolve_headers_or_err(
            &self.model_headers,
            &format!("model \"{}/{}\"", self.provider, self.model),
            lookup,
        )?;
        Ok(merge_headers(headers.as_ref(), model.as_ref()))
    }
}

/// `ModelsError("auth", "API key auth failed for provider <p>", { cause })`'s message.
fn auth_failed(provider: &str, cause: &str) -> String {
    format!("API key auth failed for provider {provider}: {cause}")
}

/// The options a request to `model` goes out with: the stored credential's headers, under the configured headers,
/// under the caller's own (`getAuth`'s `mergeHeaders(auth.headers, configuredHeaders)`, then `prepareRequest`'s
/// `mergeHeaders(auth.headers, options.headers)`), when the model carries a [`RequestConfig`]; `None` when nothing
/// changes.
pub fn request_options(model: &Model, options: &SimpleStreamOptions) -> Result<Option<SimpleStreamOptions>, String> {
    let stored = stored_request_auth(model);
    options_with(model, options, stored.as_ref())
}

fn stored_request_auth(model: &Model) -> Option<StoredRequestAuth> {
    (model.request.as_ref()?.stored_auth.as_ref()?)()
}

fn options_with(
    model: &Model,
    options: &SimpleStreamOptions,
    stored: Option<&StoredRequestAuth>,
) -> Result<Option<SimpleStreamOptions>, String> {
    let Some(request) = &model.request else {
        return Ok(None);
    };
    let configured = request.headers(&model.provider, options.base.api_key.as_deref())?;
    let auth = merge_headers(stored.and_then(|s| s.headers.as_ref()), configured.as_ref());
    if auth.is_none() {
        return Ok(None);
    }
    let mut options = options.clone();
    options.base.headers = merge_headers(auth.as_ref(), options.base.headers.as_ref());
    Ok(Some(options))
}

/// `model` with the base URL its stored OAuth credential names for this request (`result.auth.baseUrl ? { ...model,
/// baseUrl } : model`); `None` when it names none.
pub fn request_model(model: &Model) -> Option<Model> {
    model_with(model, stored_request_auth(model).as_ref())
}

fn model_with(model: &Model, stored: Option<&StoredRequestAuth>) -> Option<Model> {
    let base_url = stored?.base_url.clone()?;
    let mut model = model.clone();
    model.base_url = base_url;
    Some(model)
}

/// `prepareRequest`: [`request_model`] and [`request_options`] over one resolution of the stored credential (a
/// refresh happens at most once per request).
pub fn prepare_request(
    model: &Model,
    options: &SimpleStreamOptions,
) -> Result<(Option<Model>, Option<SimpleStreamOptions>), String> {
    let stored = stored_request_auth(model);
    let routed = model_with(model, stored.as_ref());
    let prepared = options_with(model, options, stored.as_ref())?;
    Ok((routed, prepared))
}

/// Request auth for one model: the API key and the headers its requests carry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestAuth {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
}

/// `DEFAULT_RADIUS_GATEWAY` (providers/radius-config.ts).
pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

/// models.json over the built-in catalog, with the Radius catalogs models-store.json keeps.
#[derive(Clone, Debug, Default)]
pub struct ModelRegistry {
    config: ModelConfig,
    /// models-store.json: provider id to its stored catalog (`refreshModels` restores it without the network).
    stored: Map<String, Value>,
}

impl ModelRegistry {
    pub fn new(config: ModelConfig) -> Self {
        ModelRegistry {
            config,
            stored: Map::new(),
        }
    }

    /// The registry of the models.json at `path` (`~` expands against `home`), with the models-store.json beside it;
    /// a missing file is an empty one.
    pub fn load(path: &Path, home: &Path) -> Self {
        let mut registry = Self::new(ModelConfig::load_with_home(path.to_str(), home));
        let store = path.parent().map(|dir| dir.join("models-store.json"));
        if let Some(Value::Object(stored)) = store
            .and_then(|store| std::fs::read_to_string(store).ok())
            .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok())
        {
            registry.stored = stored;
        }
        registry
    }

    /// The Radius gateway behind `provider` and the name its login shows: the built-in `radius` provider, or a
    /// models.json provider with `"oauth": "radius"` and a `baseUrl` (`configureRadiusProviders`: the gateway is the
    /// base URL without its `/v1`).
    pub fn radius_gateway(&self, provider: &str) -> Option<(String, String)> {
        let config = self.provider_config(provider);
        let custom = config.filter(|c| c.get("oauth").and_then(Value::as_str) == Some("radius"));
        match custom.and_then(|c| Some((c, c.get("baseUrl")?.as_str()?))) {
            Some((config, base_url)) => {
                let trimmed = base_url
                    .strip_suffix("/v1/")
                    .or_else(|| base_url.strip_suffix("/v1"))
                    .unwrap_or(base_url);
                let name = config.get("name").and_then(Value::as_str).unwrap_or(provider);
                Some((
                    name.to_string(),
                    crate::oauth_radius::normalize_radius_gateway_url(trimmed),
                ))
            }
            None => (provider == "radius").then(|| ("Radius".to_string(), DEFAULT_RADIUS_GATEWAY.to_string())),
        }
    }

    /// The models a provider has before models.json applies: the built-in catalog with its stored pi.dev overlay
    /// when that is newer than the bundled data (`withRemoteCatalog`); for a Radius provider, the bundled Radius
    /// catalog (only on the default gateway, under the provider's id) with its stored catalog over it
    /// (`radiusProvider.getModels`).
    fn base_models(&self, provider: &str) -> Vec<Model> {
        let Some((_, gateway)) = self.radius_gateway(provider) else {
            let builtin: Vec<Model> = crate::models::builtin_models(provider).cloned().collect();
            let overlay = crate::model_refresh::remote_overlay(
                provider,
                self.stored.get(provider),
                crate::model_refresh::builtin_generated_at(),
            );
            return crate::model_refresh::merge_models(builtin, overlay);
        };
        let models: Vec<Model> = if gateway == DEFAULT_RADIUS_GATEWAY {
            crate::models::builtin_models("radius")
                .map(|m| Model {
                    provider: provider.to_string(),
                    ..m.clone()
                })
                .collect()
        } else {
            Vec::new()
        };
        let stored = crate::model_refresh::radius_stored(provider, self.stored.get(provider));
        crate::model_refresh::merge_models(models, stored)
    }

    /// Radius providers without a stored catalog import the one their legacy OAuth credential carries
    /// (`gatewayConfig`, from the pre-models-store implementation) into `store`, as `radiusProvider.refreshModels`
    /// does on every start. `credentials` is auth.json's content.
    pub fn import_legacy_radius(&mut self, credentials: &Value, store: &config::models_store::ModelsStore, now: i64) {
        let providers: Vec<String> = self
            .provider_ids()
            .into_iter()
            .filter(|p| self.radius_gateway(p).is_some() && !self.stored.contains_key(p))
            .collect();
        for provider in providers {
            let Some(credential) = credentials.get(&provider).filter(|c| c["type"] == "oauth") else {
                continue;
            };
            let Some(legacy) = credential
                .get("gatewayConfig")
                .and_then(|config| crate::model_refresh::radius_models_from_config(&provider, config))
                .filter(|models| !models.is_empty())
            else {
                continue;
            };
            let entry = json!({"models": legacy, "checkedAt": now});
            if store.write(&provider, entry.clone()).is_ok() {
                self.stored.insert(provider, entry);
            }
        }
    }

    /// Record a refreshed catalog for `provider` (what `publish({ update })` does in the TypeScript runtime).
    pub fn set_stored(&mut self, provider: &str, entry: Value) {
        self.stored.insert(provider.to_string(), entry);
    }

    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    fn provider_config(&self, provider: &str) -> Option<&Map<String, Value>> {
        self.config.provider(provider).and_then(Value::as_object)
    }

    /// Built-in providers in catalog order, then models.json's own in file order.
    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = crate::models::builtin_providers().map(str::to_string).collect();
        for id in self.config.provider_ids() {
            if !ids.iter().any(|known| known == id) {
                ids.push(id.to_string());
            }
        }
        ids
    }

    /// `composeModelProvider(...).getModels()` for a provider models.json names: its error when the configuration
    /// does not compose.
    fn compose(&self, provider: &str, config: &Map<String, Value>) -> Result<Vec<Model>, String> {
        compose_over(provider, self.base_models(provider).iter(), config)
    }

    /// A provider's models: composed with models.json, else (no entry, or one that does not compose) built-in.
    pub fn models(&self, provider: &str) -> Vec<Model> {
        match self.provider_config(provider) {
            Some(config) => self
                .compose(provider, config)
                .unwrap_or_else(|_| self.base_models(provider)),
            None => self.base_models(provider),
        }
    }

    /// Every provider's models, providers in [`ModelRegistry::provider_ids`] order.
    pub fn all_models(&self) -> Vec<Model> {
        self.provider_ids().iter().flat_map(|p| self.models(p)).collect()
    }

    /// `getModel(provider, id)`.
    pub fn find(&self, provider: &str, id: &str) -> Option<Model> {
        match self.provider_config(provider) {
            Some(config) => self.models(provider).into_iter().find(|m| m.id == id).or_else(|| {
                // A provider whose catalog takes any id (claude-subscription): models.json applies to that model.
                let dynamic = crate::models::builtin_model(provider, id)?;
                compose_over(provider, std::iter::once(dynamic), config)
                    .ok()?
                    .into_iter()
                    .find(|m| m.id == id)
            }),
            None => self
                .base_models(provider)
                .into_iter()
                .find(|m| m.id == id)
                .or_else(|| crate::models::builtin_model(provider, id).cloned()),
        }
    }

    /// `resolveCliModel` against every model (`--provider`, `--model` with fuzzy ids, `provider/pattern`,
    /// `<pattern>:<thinking>`, ambiguity across providers, the custom-id fallback); `has_auth` says which providers
    /// have configured credentials. The dynamic `claude-subscription` catalog (any valid id, nothing listed) resolves
    /// its references directly.
    pub fn resolve_cli(
        &self,
        cli_provider: Option<&str>,
        cli_model: &str,
        cli_thinking: Option<&str>,
        has_auth: &dyn Fn(&str) -> bool,
    ) -> crate::model_resolver::CliModel {
        if let Some(model) = self.resolve_dynamic(cli_provider, cli_model) {
            return crate::model_resolver::CliModel {
                model: Some(model),
                ..Default::default()
            };
        }
        crate::model_resolver::resolve_cli_model(
            cli_provider,
            Some(cli_model),
            cli_thinking,
            &self.all_models(),
            has_auth,
        )
    }

    /// A `claude-subscription/<id>` reference (or `--provider claude-subscription --model <id>`).
    fn resolve_dynamic(&self, cli_provider: Option<&str>, cli_model: &str) -> Option<Model> {
        let dynamic = crate::claude_subscription::PROVIDER;
        fn prefixed<'t>(text: &'t str, dynamic: &str) -> Option<&'t str> {
            let head = text.get(..dynamic.len() + 1)?;
            head.eq_ignore_ascii_case(&format!("{dynamic}/"))
                .then(|| &text[dynamic.len() + 1..])
        }
        let id = match cli_provider {
            Some(p) if p.eq_ignore_ascii_case(dynamic) => prefixed(cli_model, dynamic).unwrap_or(cli_model),
            Some(_) => return None,
            None => prefixed(cli_model, dynamic)?,
        };
        if self.models(dynamic).iter().any(|m| m.id.eq_ignore_ascii_case(id)) {
            return None;
        }
        self.find(dynamic, id)
    }

    /// [`ModelRegistry::resolve_cli`] without credentials or a thinking flag: the model, or the error message.
    pub fn resolve(&self, cli_provider: Option<&str>, cli_model: &str) -> Result<Model, String> {
        let resolved = self.resolve_cli(cli_provider, cli_model, None, &|_| false);
        match (resolved.model, resolved.error) {
            (Some(model), _) => Ok(model),
            (None, error) => Err(error.unwrap_or_else(|| format!("Model \"{cli_model}\" not found"))),
        }
    }

    /// `getError` without availability: the models.json load error, then each provider's composition error.
    pub fn error(&self) -> Option<String> {
        let mut errors: Vec<String> = self.config.error().map(str::to_string).into_iter().collect();
        for provider in self.provider_ids() {
            if let Some(config) = self.provider_config(&provider)
                && let Err(error) = self.compose(&provider, config)
            {
                errors.push(format!("Provider \"{provider}\": {error}"));
            }
        }
        (!errors.is_empty()).then(|| errors.join("\n\n"))
    }

    /// The provider's models.json `apiKey` when its configuration composes.
    fn raw_key(&self, provider: &str) -> Option<&str> {
        let config = self.provider_config(provider)?;
        self.compose(provider, config).ok()?;
        config.get("apiKey").and_then(Value::as_str)
    }

    /// `checkAuth` without stored credentials: a configured key counts when it is a command or every variable it
    /// names is set; otherwise `inherited` (the built-in provider's own check).
    pub fn auth_configured(&self, provider: &str, lookup: Lookup<'_>, inherited: bool) -> bool {
        match self.raw_key(provider) {
            Some(raw) if is_command(raw) => true,
            Some(raw) => env_names(raw)
                .iter()
                .all(|name| lookup(name).is_some_and(|v| !v.trim().is_empty())),
            None => inherited,
        }
    }

    /// The key requests of `provider` use: `override_key` (`--api-key`), else the models.json `apiKey` resolved
    /// (an error when it does not resolve; no fallback to the environment), else `inherited` (the built-in key).
    pub fn api_key(
        &self,
        provider: &str,
        override_key: Option<&str>,
        lookup: Lookup<'_>,
        inherited: Option<String>,
    ) -> Result<Option<String>, String> {
        if let Some(key) = override_key {
            return Ok(Some(key.to_string()));
        }
        match self.raw_key(provider) {
            Some(raw) => resolve_config_value_or_err(raw, &format!("API key for provider \"{provider}\""), lookup)
                .map(Some)
                .map_err(|e| auth_failed(provider, &e)),
            None => Ok(inherited),
        }
    }

    /// The [`RequestConfig`] of `model` (None when models.json has nothing for its provider): the raw headers and
    /// `authHeader`, resolved with `lookup` at each request.
    pub fn request_config(&self, model: &Model, lookup: Option<SharedLookup>) -> Option<RequestConfig> {
        let config = self.provider_config(&model.provider)?;
        let strings = |value: Option<&Value>| -> Vec<(String, String)> {
            value
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        };
        // `rawModelHeaders`: the override's headers under the definition's (object spread).
        let mut model_headers: Vec<(String, String)> = strings(
            config
                .get("modelOverrides")
                .and_then(|o| o.get(&model.id))
                .and_then(|o| o.get("headers")),
        );
        let definition = config
            .get("models")
            .and_then(Value::as_array)
            .and_then(|models| models.iter().find(|m| m["id"].as_str() == Some(model.id.as_str())));
        for (key, value) in strings(definition.and_then(|d| d.get("headers"))) {
            match model_headers.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = value,
                None => model_headers.push((key, value)),
            }
        }
        Some(RequestConfig {
            provider: model.provider.clone(),
            model: model.id.clone(),
            provider_headers: strings(config.get("headers")),
            model_headers,
            auth_header: config.get("authHeader").and_then(Value::as_bool).unwrap_or(false),
            lookup,
            stored_auth: None,
        })
    }

    /// `model` ready for requests: its [`RequestConfig`] attached, resolving with `lookup` per request.
    pub fn for_requests(&self, mut model: Model, lookup: SharedLookup) -> Model {
        model.request = self.request_config(&model, Some(lookup));
        model
    }

    /// `getAuth(model, {apiKey})`: the key and headers a request to `model` carries; `None` when the provider has no
    /// key at all (not configured).
    pub fn request_auth(
        &self,
        model: &Model,
        override_key: Option<&str>,
        lookup: Lookup<'_>,
        inherited: Option<String>,
    ) -> Result<Option<RequestAuth>, String> {
        let Some(api_key) = self.api_key(&model.provider, override_key, lookup, inherited)? else {
            return Ok(None);
        };
        let headers = match self.request_config(model, None) {
            Some(request) => request.headers_with(&model.provider, Some(&api_key), lookup)?,
            None => None,
        };
        Ok(Some(RequestAuth {
            api_key: Some(api_key),
            headers,
        }))
    }
}

// ---------------------------------------------------------------------------------------------------- composition

/// `composeModelProvider(...).getModels()` over `base` (the provider's built-in models) for a provider models.json
/// names: its error when the configuration does not compose.
fn compose_over<'a>(
    provider: &str,
    base: impl Iterator<Item = &'a Model>,
    config: &Map<String, Value>,
) -> Result<Vec<Model>, String> {
    let base: Vec<Value> = base.map(|m| serde_json::to_value(m).unwrap_or(Value::Null)).collect();

    let models = apply_models_json(provider, base, config)?;
    let overrides = config.get("modelOverrides").and_then(Value::as_object);
    models
        .into_iter()
        .map(|model| {
            let id = model["id"].as_str().unwrap_or_default().to_string();
            let model = match overrides.and_then(|o| o.get(&id)).and_then(Value::as_object) {
                Some(ov) => apply_model_override(model, ov),
                None => model,
            };
            // ponytail: the Rust model keeps integral token counts; a fractional contextWindow is refused here.
            serde_json::from_value(model).map_err(|e| format!("Provider {provider}, model {id}: {e}"))
        })
        .collect()
}

/// A present, non-null property (`??` in TypeScript).
fn get<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    map.get(key).filter(|v| !v.is_null())
}

fn set(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            map.insert(key.to_string(), value);
        }
        None => {
            map.remove(key);
        }
    }
}

/// `{...a, ...b}` for objects (anything else spreads to nothing).
fn spread(a: Option<&Value>, b: Option<&Value>) -> Value {
    let mut out = Map::new();
    for value in [a, b].into_iter().flatten() {
        if let Some(map) = value.as_object() {
            for (k, v) in map {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// `mergeCompat`: the override's keys over the base's; routing and chat-template objects merge one level deeper.
fn merge_compat(base: Option<&Value>, override_: Option<&Value>) -> Option<Value> {
    let Some(override_) = override_ else {
        return base.cloned();
    };
    let mut merged = spread(base, Some(override_));
    let merged_map = merged.as_object_mut()?;
    for key in [
        "openRouterRouting",
        "vercelGatewayRouting",
        "chatTemplateKwargs",
        "chatTemplateArgs",
    ] {
        let base_value = base.and_then(|b| b.get(key));
        let override_value = override_.get(key);
        if base_value.is_some_and(Value::is_object) || override_value.is_some_and(Value::is_object) {
            merged_map.insert(key.to_string(), spread(base_value, override_value));
        }
    }
    Some(merged)
}

/// `mergeInputLimits`.
fn merge_input_limits(base: Option<&Value>, override_: Option<&Value>) -> Option<Value> {
    let Some(override_) = override_ else {
        return base.cloned();
    };
    let mut merged = spread(base, Some(override_));
    let base_images = base.and_then(|b| b.get("images")).filter(|v| !v.is_null());
    let images = match override_.get("images").filter(|v| !v.is_null()) {
        Some(images) => {
            let mut out = spread(base_images, Some(images));
            let base_resize = base_images.and_then(|i| i.get("resize")).filter(|v| !v.is_null());
            let resize = match images.get("resize").filter(|v| !v.is_null()) {
                Some(resize) => Some(spread(base_resize, Some(resize))),
                None => base_resize.cloned(),
            };
            if let Some(map) = out.as_object_mut() {
                set(map, "resize", resize);
            }
            Some(out)
        }
        None => base_images.cloned(),
    };
    if let Some(map) = merged.as_object_mut() {
        set(map, "images", images);
    }
    Some(merged)
}

/// `applyModelOverride`.
fn apply_model_override(model: Value, ov: &Map<String, Value>) -> Value {
    let Value::Object(mut model) = model else {
        return model;
    };
    let keep = |model: &Map<String, Value>, key: &str| get(ov, key).or_else(|| get(model, key)).cloned();
    let name = keep(&model, "name");
    set(&mut model, "name", name);
    let reasoning = keep(&model, "reasoning");
    set(&mut model, "reasoning", reasoning);
    if let Some(map) = get(ov, "thinkingLevelMap") {
        let merged = spread(get(&model, "thinkingLevelMap"), Some(map));
        model.insert("thinkingLevelMap".into(), merged);
    }
    let input = keep(&model, "input");
    set(&mut model, "input", input);
    let limits = merge_input_limits(get(&model, "inputLimits"), get(ov, "inputLimits"));
    set(&mut model, "inputLimits", limits);
    if let Some(cost) = get(ov, "cost").and_then(Value::as_object) {
        let base = get(&model, "cost")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut merged = Map::new();
        for key in ["input", "output", "cacheRead", "cacheWrite", "tiers"] {
            if let Some(value) = get(cost, key).or_else(|| get(&base, key)) {
                merged.insert(key.into(), value.clone());
            }
        }
        model.insert("cost".into(), Value::Object(merged));
    }
    if let Some(cache) = get(ov, "promptCache") {
        let merged = spread(get(&model, "promptCache"), Some(cache));
        model.insert("promptCache".into(), merged);
    }
    for key in ["contextWindow", "maxTokens"] {
        let value = keep(&model, key);
        set(&mut model, key, value);
    }
    if let Some(params) = get(ov, "samplingParams") {
        let merged = spread(get(&model, "samplingParams"), Some(params));
        model.insert("samplingParams".into(), merged);
    }
    let compat = merge_compat(get(&model, "compat"), get(ov, "compat"));
    set(&mut model, "compat", compat);
    Value::Object(model)
}

/// `modelFromJson`: a models.json model definition, with `defaults` (a model of the same provider) filling `api`
/// and `baseUrl` the definition and provider leave out.
fn model_from_json(
    provider: &str,
    definition: &Map<String, Value>,
    config: &Map<String, Value>,
    defaults: Option<&Map<String, Value>>,
) -> Result<Value, String> {
    let id = definition.get("id").and_then(Value::as_str).unwrap_or_default();
    let pick = |key: &str| {
        get(definition, key)
            .or_else(|| get(config, key))
            .or_else(|| defaults.and_then(|d| get(d, key)))
            .cloned()
    };
    let api = pick("api").ok_or_else(|| {
        format!("Provider {provider}, model {id}: no \"api\" specified. Set at provider or model level.")
    })?;
    let base_url = pick("baseUrl")
        .ok_or_else(|| format!("Provider {provider}: \"baseUrl\" is required when defining custom models."))?;
    for (key, name) in [("contextWindow", "contextWindow"), ("maxTokens", "maxTokens")] {
        if get(definition, key).and_then(Value::as_f64).is_some_and(|n| n <= 0.0) {
            return Err(format!("Provider {provider}, model {id}: invalid {name}"));
        }
    }
    let mut model = Map::new();
    let mut put = |key: &str, value: Option<Value>| {
        if let Some(value) = value {
            model.insert(key.to_string(), value);
        }
    };
    put("id", Some(json!(id)));
    put(
        "name",
        Some(get(definition, "name").cloned().unwrap_or_else(|| json!(id))),
    );
    put("api", Some(api));
    put("provider", Some(json!(provider)));
    put("baseUrl", Some(base_url));
    put(
        "reasoning",
        Some(get(definition, "reasoning").cloned().unwrap_or(json!(false))),
    );
    put("thinkingLevelMap", get(definition, "thinkingLevelMap").cloned());
    put(
        "input",
        Some(get(definition, "input").cloned().unwrap_or(json!(["text"]))),
    );
    put("inputLimits", get(definition, "inputLimits").cloned());
    put(
        "cost",
        Some(
            get(definition, "cost")
                .cloned()
                .unwrap_or(json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0})),
        ),
    );
    put("promptCache", get(definition, "promptCache").cloned());
    put(
        "contextWindow",
        Some(get(definition, "contextWindow").cloned().unwrap_or(json!(128000))),
    );
    put(
        "maxTokens",
        Some(get(definition, "maxTokens").cloned().unwrap_or(json!(16384))),
    );
    put("samplingParams", get(definition, "samplingParams").cloned());
    put("compat", merge_compat(get(config, "compat"), get(definition, "compat")));
    Ok(Value::Object(model))
}

/// `findModelDefaults`: the model with this id, else one on `api`, else an `openai-completions` one, else the first.
fn find_defaults<'a>(models: &'a [Value], id: &str, api: Option<&str>) -> Option<&'a Map<String, Value>> {
    let find = |key: &str, value: &str| models.iter().find(|m| m[key].as_str() == Some(value));
    find("id", id)
        .or_else(|| api.and_then(|api| find("api", api)))
        .or_else(|| find("api", "openai-completions"))
        .or_else(|| models.first())
        .and_then(Value::as_object)
}

/// `applyModelsJson`: base URL and compat applied to the provider's models, then models.json's models upserted.
fn apply_models_json(provider: &str, base: Vec<Value>, config: &Map<String, Value>) -> Result<Vec<Value>, String> {
    let oauth = get(config, "oauth");
    let base_url = get(config, "baseUrl");
    if oauth.is_some() && base_url.is_none() {
        return Err(format!(
            "Provider {provider}: \"baseUrl\" is required when \"oauth\" is set."
        ));
    }
    let definitions: &[Value] = get(config, "models")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let has_overrides = get(config, "modelOverrides")
        .and_then(Value::as_object)
        .is_some_and(|o| !o.is_empty());
    if definitions.is_empty()
        && base_url.is_none()
        && get(config, "headers").is_none()
        && get(config, "compat").is_none()
        && !has_overrides
        && get(config, "apiKey").is_none()
        && oauth.is_none()
        && !config.contains_key("authHeader")
    {
        return Err(format!(
            "Provider {provider}: must specify \"baseUrl\", \"headers\", \"compat\", \"modelOverrides\", or \"models\"."
        ));
    }
    let radius = oauth.and_then(Value::as_str) == Some("radius");
    let mut models: Vec<Value> = base
        .into_iter()
        .map(|model| {
            let Value::Object(mut model) = model else {
                return model;
            };
            if !radius && let Some(url) = base_url {
                model.insert("baseUrl".into(), url.clone());
            }
            let compat = merge_compat(get(&model, "compat"), get(config, "compat"));
            set(&mut model, "compat", compat);
            Value::Object(model)
        })
        .collect();
    for definition in definitions {
        let Some(definition) = definition.as_object() else {
            continue;
        };
        let id = definition.get("id").and_then(Value::as_str).unwrap_or_default();
        let api = get(definition, "api")
            .or_else(|| get(config, "api"))
            .and_then(Value::as_str);
        let defaults = find_defaults(&models, id, api).cloned();
        let model = model_from_json(provider, definition, config, defaults.as_ref())?;
        match models.iter().position(|m| m["id"].as_str() == Some(id)) {
            Some(index) => models[index] = model,
            None => models.push(model),
        }
    }
    Ok(models)
}

// -------------------------------------------------------------------------------------------------- config values

/// `!command` values run a shell command; everything else is a template.
pub fn is_command(config: &str) -> bool {
    config.starts_with('!')
}

/// `getConfigValueEnvVarNames`: the variables a template names, first occurrence first.
pub fn env_names(config: &str) -> Vec<String> {
    if is_command(config) {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    for part in parse_template(config) {
        if let TemplatePart::Env(name) = part
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

fn env_value(name: &str, lookup: Lookup<'_>) -> Option<String> {
    lookup(name).filter(|v| !v.is_empty())
}

/// How long a `!command` may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// `execSync(command)` through `/bin/sh -c` with stdin and stderr ignored, for at most ten seconds: trimmed stdout,
/// or None when it fails, times out or prints nothing.
fn run_command(command: &str) -> Option<String> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let Ok(out) = rx.recv_timeout(COMMAND_TIMEOUT) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    if !child.wait().ok()?.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `resolveConfigValueUncached`: `!command` output, or the template with its variables filled (None when one is
/// unset or empty).
pub fn resolve_config_value(config: &str, lookup: Lookup<'_>) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return run_command(command);
    }
    let mut out = String::new();
    for part in parse_template(config) {
        match part {
            TemplatePart::Literal(text) => out.push_str(&text),
            TemplatePart::Env(name) => out.push_str(&env_value(&name, lookup)?),
        }
    }
    Some(out)
}

/// `resolveConfigValueOrThrow`, with its messages.
pub fn resolve_config_value_or_err(config: &str, description: &str, lookup: Lookup<'_>) -> Result<String, String> {
    if let Some(value) = resolve_config_value(config, lookup) {
        return Ok(value);
    }
    if let Some(command) = config.strip_prefix('!') {
        return Err(format!("Failed to resolve {description} from shell command: {command}"));
    }
    let missing: Vec<String> = env_names(config)
        .into_iter()
        .filter(|name| env_value(name, lookup).is_none())
        .collect();
    Err(match missing.as_slice() {
        [] => format!("Failed to resolve {description}"),
        [one] => format!("Failed to resolve {description} from environment variable: {one}"),
        many => format!(
            "Failed to resolve {description} from environment variables: {}",
            many.join(", ")
        ),
    })
}

/// `resolveHeadersOrThrow`: every header resolved; None when there are none.
pub fn resolve_headers_or_err(
    headers: &[(String, String)],
    description: &str,
    lookup: Lookup<'_>,
) -> Result<Option<ProviderHeaders>, String> {
    let mut out = ProviderHeaders::new();
    for (key, value) in headers {
        let resolved = resolve_config_value_or_err(value, &format!("{description} header \"{key}\""), lookup)?;
        out.insert(key.clone(), Some(resolved));
    }
    Ok((!out.is_empty()).then_some(out))
}
