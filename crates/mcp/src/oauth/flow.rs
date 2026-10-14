//! Discovery and the authorization flow (`oauth/discovery.ts`, `oauth/flow.ts`, `oauth/provider.ts`): protected
//! resource and authorization server metadata (with issuer validation), dynamic client registration, PKCE S256,
//! code exchange and refresh, and the stateful provider over a per-server state store.

use super::{
    OAuthError, parse_authorization_server_metadata, parse_client_information, parse_protected_resource_metadata,
    parse_tokens,
};
use crate::client::LATEST_PROTOCOL_VERSION;
use serde_json::{Map, Value, json};
use std::time::Duration;
use url::Url;

/// A blocking HTTP response: status and body text.
pub struct Fetched {
    pub status: u16,
    pub body: String,
}

/// How OAuth requests are sent: the default client, or one bounded by a timeout (refreshes).
#[derive(Clone, Default)]
pub struct Fetcher {
    pub timeout: Option<Duration>,
}

impl Fetcher {
    fn client(&self) -> Result<reqwest::blocking::Client, OAuthError> {
        let mut builder = reqwest::blocking::Client::builder();
        if let Some(timeout) = self.timeout {
            builder = builder.timeout(timeout);
        }
        builder
            .build()
            .map_err(|e| OAuthError::Network(format!("fetch failed: {e}")))
    }

    pub fn send(
        &self,
        method: reqwest::Method,
        url: &str,
        headers: &[(&str, String)],
        body: Option<String>,
    ) -> Result<Fetched, OAuthError> {
        let mut request = self.client()?.request(method, url);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request
            .send()
            .map_err(|e| OAuthError::Network(format!("fetch failed: {e}")))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .map_err(|e| OAuthError::Network(format!("fetch failed: {e}")))?;
        Ok(Fetched { status, body })
    }

    fn metadata(&self, url: &str, version: &str) -> Result<Fetched, OAuthError> {
        self.send(
            reqwest::Method::GET,
            url,
            &[
                ("Accept", "application/json".into()),
                ("MCP-Protocol-Version", version.into()),
            ],
            None,
        )
    }
}

fn ok(status: u16) -> bool {
    (200..300).contains(&status)
}

/// 4xx and 502 mean "not here", so discovery tries the next candidate URL.
fn discovery_miss(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

/// The path for `/.well-known/<kind><path>`; empty for the root path.
fn path_suffix(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn parse_url(text: &str) -> Result<Url, OAuthError> {
    Url::parse(text).map_err(|_| OAuthError::Other(format!("Invalid URL: {text}")))
}

/// `response.json()`: a body that is not JSON fails.
fn json_body(fetched: &Fetched) -> Result<Value, OAuthError> {
    serde_json::from_str(&fetched.body).map_err(|e| OAuthError::Other(e.to_string()))
}

/// `discoverProtectedResourceMetadata`.
pub fn discover_protected_resource_metadata(
    server_url: &str,
    resource_metadata_url: Option<&str>,
    fetch: &Fetcher,
) -> Result<Value, OAuthError> {
    let server = parse_url(server_url)?;
    let url = match resource_metadata_url {
        Some(url) => url.to_string(),
        None => format!(
            "{}/.well-known/oauth-protected-resource{}",
            origin(&server),
            path_suffix(server.path())
        ),
    };
    let mut response = fetch.metadata(&url, LATEST_PROTOCOL_VERSION)?;
    if resource_metadata_url.is_none() && server.path() != "/" && discovery_miss(response.status) {
        response = fetch.metadata(
            &format!("{}/.well-known/oauth-protected-resource", origin(&server)),
            LATEST_PROTOCOL_VERSION,
        )?;
    }
    if !ok(response.status) {
        return Err(OAuthError::Other(format!(
            "HTTP {} loading OAuth protected resource metadata",
            response.status
        )));
    }
    parse_protected_resource_metadata(&json_body(&response)?).map_err(OAuthError::Other)
}

/// `buildAuthorizationServerDiscoveryUrls`.
pub fn authorization_server_discovery_urls(authorization_server_url: &str) -> Result<Vec<String>, OAuthError> {
    let issuer = parse_url(authorization_server_url)?;
    let path = path_suffix(issuer.path());
    let origin = origin(&issuer);
    let mut urls = vec![
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
    ];
    if !path.is_empty() {
        urls.push(format!("{origin}{path}/.well-known/openid-configuration"));
    }
    Ok(urls)
}

/// `discoverAuthorizationServerMetadata`: the first candidate that answers; its issuer must be the server's.
pub fn discover_authorization_server_metadata(
    authorization_server_url: &str,
    fetch: &Fetcher,
    skip_issuer_validation: bool,
) -> Result<Option<Value>, OAuthError> {
    for url in authorization_server_discovery_urls(authorization_server_url)? {
        let response = fetch.metadata(&url, LATEST_PROTOCOL_VERSION)?;
        if !ok(response.status) {
            if discovery_miss(response.status) {
                continue;
            }
            let shown = Url::parse(&url).map_or(url.clone(), |u| u.to_string());
            return Err(OAuthError::Other(format!(
                "HTTP {} loading authorization server metadata from {shown}",
                response.status
            )));
        }
        let metadata = parse_authorization_server_metadata(&json_body(&response)?).map_err(OAuthError::Other)?;
        if !skip_issuer_validation {
            let trim = |s: &str| s.strip_suffix('/').unwrap_or(s).to_string();
            let issuer = metadata["issuer"].as_str().unwrap_or_default();
            if trim(issuer) != trim(authorization_server_url) {
                return Err(issuer_mismatch(authorization_server_url, Some(issuer)));
            }
        }
        return Ok(Some(metadata));
    }
    Ok(None)
}

/// `OAuthIssuerMismatchError`.
pub fn issuer_mismatch(expected: &str, received: Option<&str>) -> OAuthError {
    let received = received.map_or_else(|| "none".to_string(), |r| Value::String(r.into()).to_string());
    OAuthError::Refused(format!(
        "OAuth issuer mismatch: expected {}, received {received}",
        Value::String(expected.into())
    ))
}

/// What discovery found (`OAuthServerInfo`, saved as `OAuthDiscoveryState`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerInfo {
    pub authorization_server_url: String,
    pub metadata: Option<Value>,
    pub resource_metadata: Option<Value>,
}

impl ServerInfo {
    /// The discovery state as stored: `{authorizationServerUrl, authorizationServerMetadata?, resourceMetadata?}`.
    fn state(&self, resource_metadata_url: Option<&str>) -> Value {
        let mut state = Map::new();
        state.insert(
            "authorizationServerUrl".into(),
            Value::String(self.authorization_server_url.clone()),
        );
        if let Some(metadata) = &self.metadata {
            state.insert("authorizationServerMetadata".into(), metadata.clone());
        }
        if let Some(resource) = &self.resource_metadata {
            state.insert("resourceMetadata".into(), resource.clone());
        }
        if let Some(url) = resource_metadata_url {
            state.insert("resourceMetadataUrl".into(), Value::String(url.into()));
        }
        Value::Object(state)
    }
}

/// `discoverOAuthServerInfo`.
pub fn discover_oauth_server_info(
    server_url: &str,
    resource_metadata_url: Option<&str>,
    authorization_server_metadata_url: Option<&str>,
    fetch: &Fetcher,
    skip_issuer_validation: bool,
) -> Result<ServerInfo, OAuthError> {
    let resource_metadata = match discover_protected_resource_metadata(server_url, resource_metadata_url, fetch) {
        Ok(metadata) => Some(metadata),
        Err(OAuthError::Network(message)) => return Err(OAuthError::Network(message)),
        Err(_) => None,
    };
    if let Some(url) = authorization_server_metadata_url {
        let response = fetch.metadata(url, LATEST_PROTOCOL_VERSION)?;
        if !ok(response.status) {
            return Err(OAuthError::Other(format!(
                "HTTP {} loading authorization server metadata from {url}",
                response.status
            )));
        }
        let metadata = parse_authorization_server_metadata(&json_body(&response)?).map_err(OAuthError::Other)?;
        return Ok(ServerInfo {
            authorization_server_url: metadata["issuer"].as_str().unwrap_or_default().into(),
            metadata: Some(metadata),
            resource_metadata,
        });
    }
    let authorization_server_url = match resource_metadata
        .as_ref()
        .and_then(|m| m["authorization_servers"][0].as_str())
    {
        Some(url) => url.to_string(),
        None => format!("{}/", origin(&parse_url(server_url)?)),
    };
    Ok(ServerInfo {
        metadata: discover_authorization_server_metadata(&authorization_server_url, fetch, skip_issuer_validation)?,
        authorization_server_url,
        resource_metadata,
    })
}

/// `selectResource`: the protected resource for the server URL, which must be at or above it.
pub fn select_resource(server_url: &str, metadata: Option<&Value>) -> Result<Option<String>, OAuthError> {
    let Some(metadata) = metadata else { return Ok(None) };
    let resource = metadata["resource"].as_str().unwrap_or_default();
    let mut requested = parse_url(server_url)?;
    requested.set_fragment(None);
    let configured = parse_url(resource)?;
    let mismatch = || {
        OAuthError::Other(format!(
            "Protected resource {resource} does not match MCP server {requested}"
        ))
    };
    if origin(&requested) != origin(&configured) {
        return Err(mismatch());
    }
    let with_slash = |p: &str| {
        if p.ends_with('/') {
            p.to_string()
        } else {
            format!("{p}/")
        }
    };
    if !with_slash(requested.path()).starts_with(&with_slash(configured.path())) {
        return Err(mismatch());
    }
    Ok(Some(resource.to_string()))
}

fn loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// Credentials go to https endpoints only, except on loopback.
pub fn secure_endpoint(value: &str) -> Result<Url, OAuthError> {
    let url = parse_url(value)?;
    if url.scheme() != "https" && !loopback(url.host_str().unwrap_or_default()) {
        return Err(OAuthError::Refused(format!(
            "Refusing to send OAuth credentials to non-HTTPS endpoint {url}"
        )));
    }
    Ok(url)
}

// ---- the stateful provider (`McpOAuthProvider`) ----

/// Where a provider keeps a server's OAuth state (`McpOAuthStateStore`).
pub trait StateStore: Send + Sync {
    fn load(&self) -> Result<Option<Value>, OAuthError>;
    fn save(&self, state: &Value) -> Result<(), OAuthError>;
}

/// The default stateful provider for one exact MCP server URL.
pub struct Provider<'a> {
    pub redirect_url: String,
    pub client_metadata: Map<String, Value>,
    server_url: String,
    configured_client: Option<Value>,
    store: &'a dyn StateStore,
    on_redirect: Box<dyn Fn(&str) + 'a>,
    now: fn() -> i64,
}

/// `String(new URL(value))`.
pub fn normalized_url(value: &str) -> String {
    Url::parse(value).map_or_else(|_| value.to_string(), |u| u.to_string())
}

impl<'a> Provider<'a> {
    pub fn new(
        server_url: &str,
        redirect_url: &str,
        client_name: &str,
        client_id: Option<&str>,
        client_secret: Option<&str>,
        store: &'a dyn StateStore,
        on_redirect: Box<dyn Fn(&str) + 'a>,
    ) -> Self {
        let mut client_metadata = Map::new();
        client_metadata.insert("client_name".into(), json!(client_name));
        client_metadata.insert("redirect_uris".into(), json!([redirect_url]));
        client_metadata.insert("grant_types".into(), json!(["authorization_code", "refresh_token"]));
        client_metadata.insert("response_types".into(), json!(["code"]));
        client_metadata.insert(
            "token_endpoint_auth_method".into(),
            json!(if client_secret.is_some() {
                "client_secret_post"
            } else {
                "none"
            }),
        );
        let configured_client = client_id.map(|id| {
            let mut client = json!({ "client_id": id });
            if let Some(secret) = client_secret {
                client["client_secret"] = json!(secret);
            }
            client
        });
        Self {
            redirect_url: redirect_url.to_string(),
            client_metadata,
            server_url: normalized_url(server_url),
            configured_client,
            store,
            on_redirect,
            now: crate::oauth::store::now_ms,
        }
    }

    /// Stored state for another server URL is ignored, so credentials never leak across servers.
    fn load(&self) -> Result<Map<String, Value>, OAuthError> {
        let state = self.store.load()?;
        Ok(match state {
            Some(Value::Object(map)) if map.get("serverUrl").and_then(Value::as_str) == Some(&self.server_url) => map,
            _ => {
                let mut fresh = Map::new();
                fresh.insert("serverUrl".into(), Value::String(self.server_url.clone()));
                fresh
            }
        })
    }

    fn update(&self, change: impl FnOnce(&mut Map<String, Value>)) -> Result<(), OAuthError> {
        let mut state = self.load()?;
        change(&mut state);
        self.store.save(&Value::Object(state))
    }

    pub fn state(&self) -> Result<String, OAuthError> {
        if let Some(existing) = self
            .load()?
            .get("oauthState")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Ok(existing.to_string());
        }
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| OAuthError::Other(e.to_string()))?;
        let state: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.update(|s| {
            s.insert("oauthState".into(), json!(state));
        })?;
        Ok(state)
    }

    pub fn client_information(&self) -> Result<Option<Value>, OAuthError> {
        if let Some(client) = &self.configured_client {
            return Ok(Some(client.clone()));
        }
        Ok(self.load()?.get("clientInformation").cloned())
    }

    fn save_client_information(&self, information: &Value) -> Result<(), OAuthError> {
        if self.configured_client.is_some() {
            return Ok(());
        }
        self.update(|s| {
            s.insert("clientInformation".into(), information.clone());
        })
    }

    pub fn tokens(&self) -> Result<Option<Value>, OAuthError> {
        Ok(self.load()?.get("tokens").cloned())
    }

    fn save_tokens(&self, tokens: &Value) -> Result<(), OAuthError> {
        let expires_at = tokens["expires_in"]
            .as_f64()
            .map(|seconds| (self.now)() + (seconds * 1000.0) as i64);
        self.update(|s| {
            s.insert("tokens".into(), tokens.clone());
            match expires_at {
                Some(at) => {
                    s.insert("tokensExpireAt".into(), json!(at));
                }
                None => {
                    s.shift_remove("tokensExpireAt");
                }
            }
        })
    }

    fn save_code_verifier(&self, verifier: &str) -> Result<(), OAuthError> {
        self.update(|s| {
            s.insert("codeVerifier".into(), json!(verifier));
        })
    }

    fn code_verifier(&self) -> Result<String, OAuthError> {
        self.load()?
            .get("codeVerifier")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .ok_or_else(|| OAuthError::Other("No OAuth PKCE code verifier is stored".into()))
    }

    /// `invalidateCredentials`.
    pub fn invalidate(&self, kind: &str) -> Result<(), OAuthError> {
        self.update(|s| {
            if matches!(kind, "all" | "client") {
                s.shift_remove("clientInformation");
            }
            if matches!(kind, "all" | "tokens") {
                s.shift_remove("tokens");
                s.shift_remove("tokensExpireAt");
            }
            if matches!(kind, "all" | "verifier") {
                s.shift_remove("codeVerifier");
            }
            if matches!(kind, "all" | "discovery") {
                s.shift_remove("discovery");
            }
            if kind == "all" {
                s.shift_remove("oauthState");
            }
        })
    }

    fn discovery_state(&self) -> Result<Option<Value>, OAuthError> {
        Ok(self.load()?.get("discovery").cloned())
    }

    fn save_discovery_state(&self, discovery: Value) -> Result<(), OAuthError> {
        self.update(|s| {
            s.insert("discovery".into(), discovery);
        })
    }
}

/// `OAuthFlowOptions`.
#[derive(Clone, Default)]
pub struct FlowOptions {
    pub server_url: String,
    pub authorization_code: Option<String>,
    pub iss: Option<String>,
    pub scope: Option<String>,
    pub resource_metadata_url: Option<String>,
    pub authorization_server_metadata_url: Option<String>,
    pub skip_issuer_validation: bool,
    pub skip_refresh: bool,
    pub fetch: Fetcher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowResult {
    Authorized,
    Redirect,
}

fn select_client_auth_method(information: &Value, supported: &[String]) -> &'static str {
    let secret = information["client_secret"].as_str().is_some_and(|s| !s.is_empty());
    let hinted = information["token_endpoint_auth_method"].as_str();
    if let Some(hinted) = hinted
        && let Some(method) = ["client_secret_basic", "client_secret_post", "none"]
            .into_iter()
            .find(|m| *m == hinted)
        && (supported.is_empty() || supported.iter().any(|s| s == method))
    {
        return method;
    }
    let has = |m: &str| supported.iter().any(|s| s == m);
    if supported.is_empty() {
        return if secret { "client_secret_basic" } else { "none" };
    }
    if secret && has("client_secret_basic") {
        return "client_secret_basic";
    }
    if secret && has("client_secret_post") {
        return "client_secret_post";
    }
    if has("none") {
        return "none";
    }
    if secret { "client_secret_post" } else { "none" }
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

/// A token endpoint request (`tokenRequest`): OAuth errors are read from the body before the status.
fn token_request(
    authorization_server_url: &str,
    metadata: Option<&Value>,
    client: &Value,
    resource: Option<&str>,
    mut params: Vec<(String, String)>,
    fetch: &Fetcher,
) -> Result<Value, OAuthError> {
    let endpoint = match metadata.and_then(|m| m["token_endpoint"].as_str()) {
        Some(endpoint) => endpoint.to_string(),
        None => parse_url(authorization_server_url)?
            .join("/token")
            .map_err(|e| OAuthError::Other(e.to_string()))?
            .to_string(),
    };
    let url = secure_endpoint(&endpoint)?;
    let mut set = |key: &str, value: &str| match params.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value.to_string(),
        None => params.push((key.into(), value.into())),
    };
    if let Some(resource) = resource {
        set("resource", resource);
    }
    let mut headers = vec![
        ("Accept", "application/json".to_string()),
        ("content-type", "application/x-www-form-urlencoded".to_string()),
    ];
    let supported = metadata
        .map(|m| string_list(&m["token_endpoint_auth_methods_supported"]))
        .unwrap_or_default();
    let client_id = client["client_id"].as_str().unwrap_or_default();
    match select_client_auth_method(client, &supported) {
        "client_secret_basic" => {
            use base64::Engine;
            let secret = client["client_secret"].as_str().unwrap_or_default();
            if secret.is_empty() {
                return Err(OAuthError::Other("client_secret_basic requires a client secret".into()));
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"));
            headers.push(("Authorization", format!("Basic {encoded}")));
        }
        method => {
            set("client_id", client_id);
            if method == "client_secret_post"
                && let Some(secret) = client["client_secret"].as_str().filter(|s| !s.is_empty())
            {
                set("client_secret", secret);
            }
        }
    }
    let pairs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let body = ai::oauth_flow::form_urlencode(&pairs);
    let response = fetch.send(reqwest::Method::POST, url.as_str(), &headers, Some(body))?;
    let value: Option<Value> = serde_json::from_str(&response.body).ok();
    if let Some(error) = value.as_ref().and_then(|v| v["error"].as_str()) {
        let message = value
            .as_ref()
            .and_then(|v| v["error_description"].as_str())
            .unwrap_or(error);
        return Err(OAuthError::OAuth {
            code: error.into(),
            message: if message.is_empty() {
                error.into()
            } else {
                message.into()
            },
        });
    }
    if !ok(response.status) {
        return Err(OAuthError::OAuth {
            code: "server_error".into(),
            message: format!("HTTP {}: {}", response.status, response.body),
        });
    }
    parse_tokens(&value.unwrap_or(Value::Null)).map_err(OAuthError::Other)
}

/// `registerClient`: dynamic client registration.
fn register_client(
    authorization_server_url: &str,
    metadata: Option<&Value>,
    client_metadata: &Map<String, Value>,
    scope: Option<&str>,
    fetch: &Fetcher,
) -> Result<Value, OAuthError> {
    let endpoint = metadata.and_then(|m| m["registration_endpoint"].as_str());
    if metadata.is_some() && endpoint.is_none() {
        return Err(OAuthError::Other(
            "Authorization server does not support dynamic client registration".into(),
        ));
    }
    let url = match endpoint {
        Some(endpoint) => endpoint.to_string(),
        None => parse_url(authorization_server_url)?
            .join("/register")
            .map_err(|e| OAuthError::Other(e.to_string()))?
            .to_string(),
    };
    let mut body = client_metadata.clone();
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        body.insert("scope".into(), json!(scope));
    }
    let response = fetch.send(
        reqwest::Method::POST,
        &url,
        &[
            ("Accept", "application/json".into()),
            ("content-type", "application/json".into()),
        ],
        Some(Value::Object(body).to_string()),
    )?;
    if !ok(response.status) {
        return Err(OAuthError::Refused(format!(
            "OAuth dynamic client registration failed with status {}: {}",
            response.status, response.body
        )));
    }
    parse_client_information(&json_body(&response)?).map_err(OAuthError::Other)
}

/// `startAuthorization`: the authorization URL (PKCE S256) and the code verifier.
fn start_authorization(
    authorization_server_url: &str,
    metadata: Option<&Value>,
    client: &Value,
    redirect_url: &str,
    scope: Option<&str>,
    state: Option<&str>,
    resource: Option<&str>,
) -> Result<(String, String), OAuthError> {
    if let Some(metadata) = metadata {
        if !string_list(&metadata["response_types_supported"])
            .iter()
            .any(|t| t == "code")
        {
            return Err(OAuthError::Other(
                "Authorization server does not support authorization codes".into(),
            ));
        }
        if metadata["code_challenge_methods_supported"].is_array()
            && !string_list(&metadata["code_challenge_methods_supported"])
                .iter()
                .any(|m| m == "S256")
        {
            return Err(OAuthError::Other(
                "Authorization server does not support PKCE S256".into(),
            ));
        }
    }
    let mut url = match metadata.and_then(|m| m["authorization_endpoint"].as_str()) {
        Some(endpoint) => parse_url(endpoint)?,
        None => parse_url(authorization_server_url)?
            .join("/authorize")
            .map_err(|e| OAuthError::Other(e.to_string()))?,
    };
    let (verifier, challenge) = ai::oauth_flow::generate_pkce();
    let mut pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let mut set = |key: &str, value: &str| match pairs.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value.into(),
        None => pairs.push((key.into(), value.into())),
    };
    set("response_type", "code");
    set("client_id", client["client_id"].as_str().unwrap_or_default());
    set("code_challenge", &challenge);
    set("code_challenge_method", "S256");
    set("redirect_uri", redirect_url);
    if let Some(state) = state.filter(|s| !s.is_empty()) {
        set("state", state);
    }
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        set("scope", scope);
        if scope.split_whitespace().any(|s| s == "offline_access") {
            set("prompt", "consent");
        }
    }
    if let Some(resource) = resource {
        set("resource", resource);
    }
    url.query_pairs_mut().clear().extend_pairs(pairs.iter());
    Ok((url.to_string(), verifier))
}

/// `withScope`: a response without `scope` grants the requested one.
fn with_scope(mut tokens: Value, scope: Option<&str>) -> Value {
    if tokens.get("scope").is_none()
        && let Some(scope) = scope.filter(|s| !s.is_empty())
    {
        tokens["scope"] = json!(scope);
    }
    tokens
}

/// `stepUpScope`: the challenged scopes plus the ones granted so far.
pub fn step_up_scope(granted: Option<&str>, challenged: Option<&str>) -> Option<String> {
    let challenged = challenged.filter(|s| !s.is_empty())?;
    let mut scopes: Vec<&str> = Vec::new();
    for scope in granted.into_iter().chain([challenged]).flat_map(str::split_whitespace) {
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    Some(scopes.join(" "))
}

fn run_flow(provider: &Provider<'_>, options: &FlowOptions) -> Result<FlowResult, OAuthError> {
    let metadata_url = options
        .authorization_server_metadata_url
        .as_deref()
        .map(secure_endpoint)
        .transpose()?
        .map(|u| u.to_string());
    let cached = if metadata_url.is_some() {
        None
    } else {
        provider.discovery_state()?
    };
    let discovered = match cached.as_ref().and_then(|c| c["authorizationServerUrl"].as_str()) {
        Some(url) => {
            let cached = cached.as_ref().unwrap_or(&Value::Null);
            ServerInfo {
                authorization_server_url: url.to_string(),
                metadata: match cached.get("authorizationServerMetadata") {
                    Some(metadata) => Some(metadata.clone()),
                    None => {
                        discover_authorization_server_metadata(url, &options.fetch, options.skip_issuer_validation)?
                    }
                },
                resource_metadata: cached.get("resourceMetadata").cloned(),
            }
        }
        None => discover_oauth_server_info(
            &options.server_url,
            options.resource_metadata_url.as_deref(),
            metadata_url.as_deref(),
            &options.fetch,
            options.skip_issuer_validation,
        )?,
    };
    if metadata_url.is_none() {
        provider.save_discovery_state(discovered.state(options.resource_metadata_url.as_deref()))?;
    }
    let metadata = discovered.metadata.as_ref();
    let resource = select_resource(&options.server_url, discovered.resource_metadata.as_ref())?;
    let resource_scopes = discovered
        .resource_metadata
        .as_ref()
        .map(|m| string_list(&m["scopes_supported"]).join(" "));
    let scope = [
        options.scope.clone(),
        resource_scopes,
        provider
            .client_metadata
            .get("scope")
            .and_then(Value::as_str)
            .map(String::from),
    ]
    .into_iter()
    .flatten()
    .find(|s| !s.is_empty());
    let client = match provider.client_information()? {
        Some(client) => client,
        None => {
            if options.authorization_code.is_some() {
                return Err(OAuthError::Other(
                    "OAuth client information is missing during code exchange".into(),
                ));
            }
            let client = register_client(
                &discovered.authorization_server_url,
                metadata,
                &provider.client_metadata,
                scope.as_deref(),
                &options.fetch,
            )?;
            provider.save_client_information(&client)?;
            client
        }
    };
    if let Some(code) = &options.authorization_code {
        // RFC 9207: never send a code from another authorization server to this one.
        if let Some(metadata) = metadata
            && (options.iss.is_some() || metadata["authorization_response_iss_parameter_supported"] == true)
        {
            let issuer = metadata["issuer"].as_str().unwrap_or_default();
            if options.iss.as_deref() != Some(issuer) {
                return Err(issuer_mismatch(issuer, options.iss.as_deref()));
            }
        }
        let verifier = provider.code_verifier()?;
        let tokens = token_request(
            &discovered.authorization_server_url,
            metadata,
            &client,
            resource.as_deref(),
            vec![
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), code.clone()),
                ("code_verifier".into(), verifier),
                ("redirect_uri".into(), provider.redirect_url.clone()),
            ],
            &options.fetch,
        )?;
        provider.save_tokens(&with_scope(tokens, scope.as_deref()))?;
        return Ok(FlowResult::Authorized);
    }
    let existing = if options.skip_refresh { None } else { provider.tokens()? };
    if let Some(refresh_token) = existing.as_ref().and_then(|t| t["refresh_token"].as_str()) {
        let refreshed = token_request(
            &discovered.authorization_server_url,
            metadata,
            &client,
            resource.as_deref(),
            vec![
                ("grant_type".into(), "refresh_token".into()),
                ("refresh_token".into(), refresh_token.into()),
            ],
            &options.fetch,
        );
        match refreshed {
            Ok(tokens) => {
                // `{refresh_token, ...tokens}`: the old refresh token unless a new one came.
                let mut merged = Map::new();
                merged.insert("refresh_token".into(), json!(refresh_token));
                if let Value::Object(tokens) = tokens {
                    merged.extend(tokens);
                }
                // A refresh without `scope` keeps the scope of the grant.
                let granted = existing.as_ref().and_then(|t| t["scope"].as_str());
                provider.save_tokens(&with_scope(Value::Object(merged), granted))?;
                return Ok(FlowResult::Authorized);
            }
            Err(error @ OAuthError::Refused(_)) => return Err(error),
            Err(OAuthError::OAuth { code, message }) if code != "server_error" => {
                return Err(OAuthError::OAuth { code, message });
            }
            Err(_) => {}
        }
    }
    let state = provider.state()?;
    let (url, verifier) = start_authorization(
        &discovered.authorization_server_url,
        metadata,
        &client,
        &provider.redirect_url,
        scope.as_deref(),
        Some(&state),
        resource.as_deref(),
    )?;
    provider.save_code_verifier(&verifier)?;
    (provider.on_redirect)(&url);
    Ok(FlowResult::Redirect)
}

/// `authorizeMcp`: the flow, retried once without the client (`invalid_client`) or the tokens (`invalid_grant`).
pub fn authorize(provider: &Provider<'_>, options: &FlowOptions) -> Result<FlowResult, OAuthError> {
    match run_flow(provider, options) {
        Err(OAuthError::OAuth { code, .. }) if code == "invalid_client" || code == "unauthorized_client" => {
            provider.invalidate("all")?;
            run_flow(provider, options)
        }
        Err(OAuthError::OAuth { code, .. }) if code == "invalid_grant" => {
            provider.invalidate("tokens")?;
            run_flow(provider, options)
        }
        other => other,
    }
}
