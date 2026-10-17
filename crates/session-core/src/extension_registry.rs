//! `ctx.modelRegistry` and `ctx.scopedModels` for extension processes (core/model-registry.ts, the facade the
//! worker's `LegacyExtensionAdapter` hands the runner): the session profile's models and the auth their requests
//! carry, so an extension can make its own model calls. Requests (crates/extensions/PROTOCOL.md section 4):
//!
//! - `models/all` (`getAll`), `models/available` (`getAvailable`), `models/find {provider, id}` (`find`);
//! - `models/getApiKeyAndHeaders {provider, id}`: `{ok: true, apiKey?, headers?, baseUrl?}` or `{ok: false, error}`;
//! - `models/getApiKeyForProvider {provider}`, `models/hasConfiguredAuth {provider}`, `models/isUsingOAuth
//!   {provider}`.
//!
//! Streaming through the registry (`stream`, `complete`) and registering providers by code do not cross the process
//! boundary: the extension calls the provider itself with the key and headers it was given, and declares providers
//! in `capabilities.providers`.

use crate::{ModelFlags, RunEnv};
use ai::Model;
use ai::options::ProviderHeaders;
use serde_json::{Map, Value, json};

fn model_json(model: &Model) -> Value {
    serde_json::to_value(model).unwrap_or(Value::Null)
}

fn auth_path(env: &RunEnv) -> std::path::PathBuf {
    config::paths::AgentPaths::new(env.agent_dir()).auth()
}

/// `getAll`: every model of the profile (the built-in catalog, models.json, the extensions' providers).
pub fn all(env: &RunEnv) -> Value {
    Value::Array(crate::lane::registry(env).all_models().iter().map(model_json).collect())
}

/// `getAvailable`: the models of providers with credentials.
pub fn available(env: &RunEnv) -> Value {
    Value::Array(crate::lane::available_models(env).iter().map(model_json).collect())
}

/// `find(provider, modelId)`; `null` when the profile has no such model.
pub fn find(env: &RunEnv, provider: &str, id: &str) -> Value {
    crate::lane::registry(env)
        .find(provider, id)
        .map_or(Value::Null, |model| model_json(&model))
}

/// `hasConfiguredAuth`: a stored credential, the models.json key, or the built-in provider's environment key.
pub fn has_configured_auth(env: &RunEnv, provider: &str) -> bool {
    if config::auth::read_stored_credential(&auth_path(env), provider).is_some() {
        return true;
    }
    let registry = crate::lane::registry(env);
    registry.auth_configured(provider, &*env.var, crate::lane::env_api_key(env, provider).is_some())
}

/// `isUsingOAuth`: the provider's stored credential is an OAuth one.
pub fn is_using_oauth(env: &RunEnv, provider: &str) -> bool {
    config::auth::read_stored_credential(&auth_path(env), provider)
        .is_some_and(|c| c.get("type").and_then(Value::as_str) == Some("oauth"))
}

/// `getApiKeyForProvider`: the key a request of `provider` carries; `null` when there is none or it fails to
/// resolve.
pub fn api_key_for_provider(env: &RunEnv, flags: &ModelFlags, provider: &str) -> Value {
    crate::lane::api_key(flags, env, provider).map_or(Value::Null, Value::from)
}

fn headers_json(headers: Option<ProviderHeaders>) -> Option<Value> {
    let headers: Map<String, Value> = headers?
        .into_iter()
        .filter_map(|(name, value)| Some((name, Value::from(value?))))
        .collect();
    Some(Value::Object(headers))
}

/// `getApiKeyAndHeaders(model)`: the key, headers and base URL a request to the model carries, resolved now (a
/// `!command` key runs once). An error is the cause's message; a missing key under `authHeader` reads
/// `No API key found for "<provider>"`. Without any credential the model's static and configured headers are
/// returned (`getCompatibilityRequestConfig`).
pub fn api_key_and_headers(env: &RunEnv, flags: &ModelFlags, provider: &str, id: &str) -> Value {
    let registry = crate::lane::registry(env);
    let Some(found) = registry.find(provider, id) else {
        return json!({"ok": false, "error": format!("Model \"{provider}/{id}\" not found")});
    };
    let model = crate::lane::for_requests(&registry, found, env);
    let missing = || json!({"ok": false, "error": format!("No API key found for \"{provider}\"")});
    let failed = |error: String| {
        let prefix = format!("API key auth failed for provider {provider}: ");
        let cause = error.strip_prefix(&prefix).unwrap_or(&error);
        if cause == "authHeader requires a resolved API key" {
            return missing();
        }
        json!({"ok": false, "error": cause})
    };
    let key = match crate::lane::request_key(flags, env, provider) {
        Ok(key) => key,
        Err(error) => return failed(error),
    };
    let request = model.request.clone().unwrap_or_default();
    let stored = crate::stored_auth::oauth_request_auth(env, provider);
    let stored_headers = stored.as_ref().and_then(|s| s.headers.clone());
    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    if key.is_none() && stored_headers.is_none() {
        // `getAuth` resolves nothing: the compatibility config (a provider that needs a key fails as missing).
        let configured = match request.headers(provider, None) {
            Ok(headers) => headers,
            Err(error) => return failed(error),
        };
        let fixed: Option<ProviderHeaders> = model
            .headers
            .as_ref()
            .map(|h| h.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect());
        if let Some(headers) = headers_json(ai::auth::merge_headers(fixed.as_ref(), configured.as_ref())) {
            out.insert("headers".into(), headers);
        }
        return Value::Object(out);
    }
    let configured = match request.headers(provider, key.as_deref()) {
        Ok(headers) => headers,
        Err(error) => return failed(error),
    };
    if let Some(key) = key {
        out.insert("apiKey".into(), key.into());
    }
    if let Some(headers) = headers_json(ai::auth::merge_headers(stored_headers.as_ref(), configured.as_ref())) {
        out.insert("headers".into(), headers);
    }
    if let Some(base_url) = stored.and_then(|s| s.base_url).filter(|u| !u.is_empty()) {
        out.insert("baseUrl".into(), base_url.into());
    }
    Value::Object(out)
}

/// `ctx.scopedModels`: the `--models` patterns, else the settings' `enabledModels`, resolved against the available
/// models (`resolveModelScope`): `[{model, thinkingLevel?}]`, empty without scoping.
pub fn scoped_models(env: &RunEnv, flags: &ModelFlags) -> Value {
    let patterns: Vec<String> = match &flags.models {
        Some(patterns) => patterns.clone(),
        None => config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false)
            .settings()
            .get("enabledModels")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
    };
    if patterns.is_empty() {
        return json!([]);
    }
    let available = crate::lane::available_models(env);
    let (scoped, _) = ai::model_resolver::resolve_model_scope(&patterns, &available);
    Value::Array(
        scoped
            .iter()
            .map(|s| {
                let mut item = json!({"model": model_json(&s.model)});
                if let Some(level) = &s.thinking_level {
                    item["thinkingLevel"] = level.clone().into();
                }
                item
            })
            .collect(),
    )
}

/// Serves a `models/*` request.
pub fn request(env: &RunEnv, flags: &ModelFlags, method: &str, params: &Value) -> Option<Result<Value, String>> {
    let text = |key: &str| -> Result<&str, String> {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("\"{key}\" must be a string"))
    };
    let answer = match method {
        "models/all" => Ok(all(env)),
        "models/available" => Ok(available(env)),
        "models/find" => text("provider").and_then(|p| Ok(find(env, p, text("id")?))),
        "models/getApiKeyAndHeaders" => {
            text("provider").and_then(|p| Ok(api_key_and_headers(env, flags, p, text("id")?)))
        }
        "models/getApiKeyForProvider" => text("provider").map(|p| api_key_for_provider(env, flags, p)),
        "models/hasConfiguredAuth" => text("provider").map(|p| json!(has_configured_auth(env, p))),
        "models/isUsingOAuth" => text("provider").map(|p| json!(is_using_oauth(env, p))),
        _ => return None,
    };
    Some(answer)
}
