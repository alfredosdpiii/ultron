//! The auth.json credential a request uses (`resolveProviderAuth` over the composed provider auth of
//! `provider-composer.ts`): a stored credential owns its provider. An `api_key` credential's key (its `$VAR` template
//! resolved against the credential's `env`, then the environment) wins over the models.json `apiKey` and the
//! environment; a credential without a usable key falls back to the built-in provider's environment key (a custom
//! provider has none). An `oauth` credential gives its access token, refreshed under the store's lock when it expires
//! within five minutes, for the providers with an OAuth handler; any other credential type leaves the provider
//! unconfigured (no environment fallback).

use crate::RunEnv as Env;
use ai::auth::{AuthContext, AuthOverrides, FileCredentialStore, ProviderAuth};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// What auth.json says about one provider's request key.
#[derive(Debug, PartialEq)]
pub enum Stored {
    /// No credential stored: models.json and the environment decide.
    Nothing,
    /// The credential owns the provider: this key, or none at all.
    Owns(Option<String>),
}

/// The stored credential of `provider` in the profile's auth.json. A file that is not JSON fails the request the
/// way the TypeScript store's read does.
pub fn stored_key(env: &Env, provider: &str) -> Result<Stored, String> {
    let path = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Stored::Nothing);
    };
    let data = config::json::parse(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("Credential store read failed for {provider}: {e}"))?;
    let Some(credential) = data.get(provider).filter(|c| !c.is_null()).cloned() else {
        return Ok(Stored::Nothing);
    };
    match credential.get("type").and_then(Value::as_str) {
        Some("api_key") => {
            let var = env.var.clone();
            let resolved = config::auth::resolve_stored_api_key(credential, &move |name: &str| var(name))
                .map_err(|e| format!("Credential store read failed for {provider}: {e}"))?;
            let key = resolved
                .as_ref()
                .and_then(|c| c.get("key"))
                .and_then(Value::as_str)
                .filter(|k| !k.is_empty())
                .map(str::to_string);
            // `envApiKeyAuth.resolve`: no stored key, so the built-in provider's ambient key; a custom provider
            // (no inherited auth) is not configured.
            let builtin = ai::models::builtin_providers().any(|p| p == provider)
                || crate::lane::registry(env).radius_gateway(provider).is_some();
            Ok(Stored::Owns(key.or_else(|| {
                builtin.then(|| crate::lane::env_api_key(env, provider)).flatten()
            })))
        }
        Some("oauth") => oauth_auth(env, provider, path, None, &OAuthOptions::for_env(env))
            .map(|auth| Stored::Owns(auth.and_then(|a| a.api_key))),
        _ => Ok(Stored::Owns(None)),
    }
}

/// The credential of `provider`'s stored OAuth credential (`getAuthCredential`: its key or bearer token), refreshed when it has less than `min_validity_ms` (five
/// minutes by default) left; `None` when nothing usable is stored.
pub fn oauth_access(env: &Env, provider: &str, min_validity_ms: Option<f64>) -> Result<Option<String>, String> {
    oauth_access_with(env, provider, min_validity_ms, &OAuthOptions::for_env(env))
}

/// [`oauth_access`] over `options` (the transport token refreshes go through).
pub fn oauth_access_with(
    env: &Env,
    provider: &str,
    min_validity_ms: Option<f64>,
    options: &OAuthOptions,
) -> Result<Option<String>, String> {
    let path = config::paths::AgentPaths::new(env.agent_dir()).auth();
    Ok(oauth_auth(env, provider, path, min_validity_ms, options)?.and_then(|auth| credential_of(&auth)))
}

/// `getAuthCredential`: the API key, else the token of an `Authorization: Bearer` header (Kimi's OAuth auth).
fn credential_of(auth: &ai::auth::ModelAuth) -> Option<String> {
    if let Some(key) = auth.api_key.as_ref().filter(|k| !k.is_empty()) {
        return Some(key.clone());
    }
    let header = auth
        .headers
        .iter()
        .flatten()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| value.clone())?;
    let (scheme, token) = header.split_once(char::is_whitespace)?;
    let token = token.trim_start();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_string())
}

/// What a stored OAuth credential adds to a request beyond its key (`ModelAuth`): the base URL it names (GitHub
/// Copilot's token proxy endpoint, else its enterprise or individual endpoint) and its headers (Kimi For Coding's
/// `Authorization: Bearer <access>`), refreshed first when the token expires soon; `None` for any other credential.
pub fn oauth_request_auth(env: &Env, provider: &str) -> Option<ai::model_registry::StoredRequestAuth> {
    let path = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let stored = config::auth::read_stored_credential(&path, provider)?;
    if stored.get("type").and_then(Value::as_str) != Some("oauth") {
        return None;
    }
    let auth = oauth_auth(env, provider, path, None, &OAuthOptions::for_env(env))
        .ok()
        .flatten()?;
    Some(ai::model_registry::StoredRequestAuth {
        base_url: auth.base_url,
        headers: auth.headers,
    })
}

/// How an OAuth handler reaches its provider: the transport, and the port a browser login's callback server listens
/// on instead of the registered one (`Some(0)`: any free port; tests).
#[derive(Clone)]
pub struct OAuthOptions {
    pub transport: Arc<dyn ai::Transport>,
    pub callback_port: Option<u16>,
}

impl OAuthOptions {
    /// The options of a run in `env`: its proxies (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`) reach the providers.
    pub fn for_env(env: &Env) -> OAuthOptions {
        OAuthOptions {
            transport: Arc::new(ai::ReqwestTransport::for_env(&*env.var, None)),
            callback_port: None,
        }
    }
}

impl Default for OAuthOptions {
    fn default() -> Self {
        OAuthOptions {
            transport: Arc::new(ai::ReqwestTransport::new(None)),
            callback_port: None,
        }
    }
}

/// The OAuth handler of `provider`: a built-in provider's, or Radius's on the provider's gateway (the built-in
/// `radius`, or a models.json provider with `"oauth": "radius"`).
pub fn oauth_login_handler(env: &Env, provider: &str) -> Option<Arc<dyn ai::auth::OAuthAuth>> {
    oauth_handler_with(env, provider, &OAuthOptions::for_env(env))
}

/// [`oauth_login_handler`] over `options`: every provider TypeScript signs in with an account (Anthropic, OpenAI
/// Codex, GitHub Copilot, xAI, Kimi For Coding, Meta, OpenRouter, Radius).
pub fn oauth_handler_with(env: &Env, provider: &str, options: &OAuthOptions) -> Option<Arc<dyn ai::auth::OAuthAuth>> {
    let transport = options.transport.clone();
    let clock = ai::oauth_flow::system_clock;
    if let Some((name, gateway)) = crate::lane::registry(env).radius_gateway(provider) {
        let mut radius = ai::oauth_radius::RadiusOAuth::new(&name, &gateway, transport, clock);
        if let Some(port) = options.callback_port {
            radius = radius.with_callback_port(port);
        }
        return Some(Arc::new(radius));
    }
    Some(match provider {
        "xai" => Arc::new(ai::oauth_xai::XaiOAuth { transport, clock }),
        "kimi-coding" => Arc::new(ai::oauth_kimi::KimiCodingOAuth { transport, clock }),
        "meta" => Arc::new(ai::oauth_meta::MetaOAuth { transport, clock }),
        "openrouter" => Arc::new(ai::oauth_openrouter::OpenRouterOAuth { transport }),
        _ => Arc::new(ai::auth::BuiltinOAuth {
            provider: ai::oauth::OAuthProvider::from_id(provider)?,
            transport,
            clock,
            callback_port: options.callback_port,
        }),
    })
}

/// The request auth of a stored OAuth credential (`resolveStoredOAuth`, then `toAuth`: the access token, and a base
/// URL for some providers), refreshed through the provider's token endpoint when it expires soon. Providers without
/// an OAuth handler are not configured by it.
fn oauth_auth(
    env: &Env,
    provider: &str,
    path: PathBuf,
    min_validity_ms: Option<f64>,
    options: &OAuthOptions,
) -> Result<Option<ai::auth::ModelAuth>, String> {
    let Some(oauth) = oauth_handler_with(env, provider, options) else {
        return Ok(None);
    };
    let auth = ProviderAuth {
        api_key: None,
        oauth: Some(oauth),
    };
    let ctx = EnvContext(env.clone());
    let store = FileCredentialStore::new(path);
    // Callers are synchronous and may sit inside a runtime: the resolution runs on its own thread and runtime.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?;
                runtime
                    .block_on(ai::auth::resolve_provider_auth(
                        provider,
                        &auth,
                        &store,
                        &ctx,
                        &AuthOverrides {
                            min_oauth_validity_ms: min_validity_ms,
                            ..AuthOverrides::default()
                        },
                    ))
                    .map(|result| result.map(|r| r.auth))
                    .map_err(|e| e.message())
            })
            .join()
            .unwrap_or_else(|_| Err(format!("OAuth auth failed for {provider}")))
    })
}

/// The session's environment as the auth context (`defaultProviderAuthContext` over the injected lookup).
struct EnvContext(Env);

impl AuthContext for EnvContext {
    fn env(&self, name: &str) -> Option<String> {
        (self.0.var)(name).filter(|v| !v.trim().is_empty())
    }

    fn file_exists(&self, path: &str) -> bool {
        match path.strip_prefix('~') {
            Some(rest) => PathBuf::from(format!("{}{rest}", self.0.home.display())).exists(),
            None => PathBuf::from(path).exists(),
        }
    }
}
