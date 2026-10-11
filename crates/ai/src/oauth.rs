//! OAuth refresh and request-auth derivation for the subscription providers of `auth/oauth/*.ts`: Anthropic (Claude
//! Pro/Max), OpenAI Codex (ChatGPT) and GitHub Copilot. Network goes through [`Transport`]; `now_ms` replaces
//! `Date.now()`. Credentials keep the stored `auth.json` shape (`{"type":"oauth", refresh, access, expires, ...}`).
//!
//! Evidence: `fixtures/generate-rest.mjs oauth` (its `subscription` cases) drives the TypeScript `OAuthAuth.refresh`/`toAuth` through a stubbed
//! `fetch`; `tests/oauth.rs` replays the recorded responses here and compares requests and results.

pub use crate::auth::{ModelAuth, OAuthCredential};
use crate::http::{AbortSignal, HeaderMap, HttpRequest, Transport};
use crate::json::{parse_json, stringify};
use crate::oauth_flow::form_urlencode;
use serde_json::{Map, Value, json};

pub(crate) const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(crate) const ANTHROPIC_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(crate) const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
pub(crate) const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("user-agent", "GitHubCopilotChat/0.35.0"),
    ("editor-version", "vscode/1.107.0"),
    ("editor-plugin-version", "copilot-chat/0.35.0"),
    ("copilot-integration-id", "vscode-chat"),
];
pub(crate) const COPILOT_API_VERSION: &str = "2026-06-01";
pub(crate) const COPILOT_INDIVIDUAL_URL: &str = "https://api.individual.githubcopilot.com";
pub(crate) const FIVE_MINUTES_MS: f64 = 5.0 * 60.0 * 1000.0;

/// Which OAuth flow a credential belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthProvider {
    Anthropic,
    OpenAiCodex,
    GitHubCopilot,
}

impl OAuthProvider {
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "anthropic" => Some(Self::Anthropic),
            "openai-codex" => Some(Self::OpenAiCodex),
            "github-copilot" => Some(Self::GitHubCopilot),
            _ => None,
        }
    }

    /// `OAuthAuth.refresh`: exchanges the refresh token for a new credential. `Err` is the upstream error message.
    pub async fn refresh(
        self,
        transport: &dyn Transport,
        credential: &OAuthCredential,
        now_ms: f64,
        signal: Option<&AbortSignal>,
    ) -> Result<OAuthCredential, String> {
        match self {
            Self::Anthropic => refresh_anthropic(transport, &credential.refresh, now_ms, signal).await,
            Self::OpenAiCodex => refresh_codex(transport, &credential.refresh, now_ms, signal).await,
            Self::GitHubCopilot => {
                let domain = copilot_enterprise_domain(credential);
                refresh_copilot(transport, &credential.refresh, domain.as_deref(), signal).await
            }
        }
    }

    /// `OAuthAuth.toAuth`.
    pub fn to_auth(self, credential: &OAuthCredential) -> ModelAuth {
        let base_url = (self == Self::GitHubCopilot).then(|| {
            copilot_base_url(
                Some(&credential.access),
                copilot_enterprise_domain(credential).as_deref(),
            )
        });
        ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: None,
            base_url,
        }
    }
}

pub(crate) struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub text: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn status_text(&self) -> &'static str {
        status_text(self.status)
    }
}

/// Why a request produced no response: the signal aborted it (`AbortError`) or the transport failed (`TypeError`).
#[derive(Debug)]
pub(crate) enum SendError {
    Aborted,
    Network(String),
}

impl SendError {
    pub fn message(&self) -> String {
        match self {
            Self::Aborted => "This operation was aborted".into(),
            Self::Network(message) => message.clone(),
        }
    }

    /// `${error.name}: ${error.message}`.
    pub fn detail(&self) -> String {
        match self {
            Self::Aborted => format!("AbortError: {}", self.message()),
            Self::Network(message) => format!("TypeError: {message}"),
        }
    }
}

pub(crate) async fn send(
    transport: &dyn Transport,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: String,
    signal: Option<&AbortSignal>,
) -> Result<Reply, SendError> {
    if signal.is_some_and(AbortSignal::aborted) {
        return Err(SendError::Aborted);
    }
    let headers: HeaderMap = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), (*v).to_owned()))
        .collect();
    let request = HttpRequest {
        method: method.into(),
        url: url.into(),
        headers,
        body,
    };
    let mut response = transport.send(request).await.map_err(SendError::Network)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.body.next_chunk().await.map_err(SendError::Network)? {
        bytes.extend(chunk);
    }
    Ok(Reply {
        status: response.status,
        headers: response.headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

pub(crate) fn status_text(status: u16) -> &'static str {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("")
}

async fn refresh_anthropic(
    transport: &dyn Transport,
    refresh: &str,
    now_ms: f64,
    signal: Option<&AbortSignal>,
) -> Result<OAuthCredential, String> {
    let body = json!({"grant_type": "refresh_token", "client_id": ANTHROPIC_CLIENT_ID, "refresh_token": refresh});
    anthropic_token_request(
        transport,
        &body,
        now_ms,
        signal,
        |details| format!("Anthropic token refresh request failed. url={ANTHROPIC_TOKEN_URL}; details={details}"),
        "Anthropic token refresh",
    )
    .await
}

/// `postJson` to the Anthropic token endpoint plus the credential built from its reply. `failed` wraps request
/// failures; `lead` starts the invalid-JSON and missing-field messages.
pub(crate) async fn anthropic_token_request(
    transport: &dyn Transport,
    body: &Value,
    now_ms: f64,
    signal: Option<&AbortSignal>,
    failed: impl Fn(String) -> String,
    lead: &str,
) -> Result<OAuthCredential, String> {
    let headers = [("Content-Type", "application/json"), ("Accept", "application/json")];
    let reply = send(
        transport,
        "POST",
        ANTHROPIC_TOKEN_URL,
        &headers,
        stringify(body),
        signal,
    )
    .await
    .map_err(|e| failed(e.detail()))?;
    if !reply.ok() {
        return Err(failed(format!(
            "Error: HTTP request failed. status={}; url={ANTHROPIC_TOKEN_URL}; body={}",
            reply.status, reply.text
        )));
    }
    let data = parse_json(&reply.text).map_err(|e| {
        format!(
            "{lead} returned invalid JSON. url={ANTHROPIC_TOKEN_URL}; body={}; details=SyntaxError: {e}",
            reply.text
        )
    })?;
    // ponytail: upstream stores `undefined`/NaN for missing fields; here they are errors.
    let field = |name: &str| data.get(name).and_then(Value::as_str).map(str::to_owned);
    let (Some(access), Some(refresh), Some(expires_in)) = (
        field("access_token"),
        field("refresh_token"),
        data.get("expires_in").and_then(Value::as_f64),
    ) else {
        return Err(format!("{lead} response missing fields: {}", stringify(&data)));
    };
    Ok(OAuthCredential {
        refresh,
        access,
        expires: now_ms + expires_in * 1000.0 - FIVE_MINUTES_MS,
        extra: Map::new(),
    })
}

async fn refresh_codex(
    transport: &dyn Transport,
    refresh: &str,
    now_ms: f64,
    signal: Option<&AbortSignal>,
) -> Result<OAuthCredential, String> {
    let body = form_urlencode(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", CODEX_CLIENT_ID),
    ]);
    let headers = [("Content-Type", "application/x-www-form-urlencoded")];
    let reply = send(transport, "POST", CODEX_TOKEN_URL, &headers, body, signal)
        .await
        .map_err(|e| format!("OpenAI Codex token refresh error: {}", e.message()))?;
    codex_credential(&reply, "refresh", now_ms)
}

/// `readTokenResponse` + `credentialsFromToken`; `operation` is "exchange" or "refresh".
pub(crate) fn codex_credential(reply: &Reply, operation: &str, now_ms: f64) -> Result<OAuthCredential, String> {
    if !reply.ok() {
        let text = if reply.text.is_empty() {
            reply.status_text()
        } else {
            &reply.text
        };
        return Err(format!(
            "OpenAI Codex token {operation} failed ({}): {text}",
            reply.status
        ));
    }
    let data = parse_json(&reply.text).map_err(|e| format!("SyntaxError: {e}"))?;
    let non_empty = |name: &str| {
        data.get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let (Some(access), Some(refresh), Some(expires_in)) = (
        non_empty("access_token"),
        non_empty("refresh_token"),
        data.get("expires_in").and_then(Value::as_f64),
    ) else {
        return Err(format!(
            "OpenAI Codex token {operation} response missing fields: {}",
            stringify(&data)
        ));
    };
    let account_id = codex_account_id(&access).ok_or("Failed to extract accountId from token")?;
    let mut extra = Map::new();
    extra.insert("accountId".into(), json!(account_id));
    Ok(OAuthCredential {
        refresh,
        access,
        expires: now_ms + expires_in * 1000.0,
        extra,
    })
}

/// The ChatGPT account id in a Codex access token's JWT payload (`getAccountId`).
pub fn codex_account_id(access_token: &str) -> Option<String> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    // `atob` yields one char per byte; JSON.parse reads that Latin-1 string.
    let decoded = crate::jsutil::latin1(&crate::jsutil::atob(parts[1])?);
    let payload = parse_json(&decoded).ok()?;
    let id = payload.get(CODEX_JWT_CLAIM_PATH)?.get("chatgpt_account_id")?.as_str()?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// `normalizeDomain`: the lower-cased host of a GitHub Enterprise URL or bare domain.
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let url = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    reqwest::Url::parse(&url).ok()?.host_str().map(str::to_owned)
}

pub(crate) fn copilot_enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    let url = credential
        .extra
        .get("enterpriseUrl")?
        .as_str()
        .filter(|s| !s.is_empty())?;
    normalize_domain(url)
}

/// `getGitHubCopilotBaseUrl`: the token's `proxy-ep` host (as `api.`), else the enterprise or individual endpoint.
pub fn copilot_base_url(token: Option<&str>, enterprise_domain: Option<&str>) -> String {
    let from_token = token.filter(|t| !t.is_empty()).and_then(|t| {
        let host = t
            .match_indices("proxy-ep=")
            .find_map(|(i, m)| t[i + m.len()..].split(';').next().filter(|h| !h.is_empty()))?;
        Some(format!(
            "https://{}",
            host.strip_prefix("proxy.")
                .map_or(host.to_owned(), |rest| format!("api.{rest}"))
        ))
    });
    from_token.unwrap_or_else(|| match enterprise_domain.filter(|d| !d.is_empty()) {
        Some(domain) => format!("https://copilot-api.{domain}"),
        None => COPILOT_INDIVIDUAL_URL.to_owned(),
    })
}

/// `fetchJson`: GET/POST, `${status} ${statusText}: ${text}` on a non-2xx reply, then the parsed body.
pub(crate) async fn fetch_copilot_json(
    transport: &dyn Transport,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: String,
    signal: Option<&AbortSignal>,
) -> Result<Value, String> {
    let reply = send(transport, method, url, headers, body, signal)
        .await
        .map_err(|e| e.message())?;
    if !reply.ok() {
        return Err(format!("{} {}: {}", reply.status, reply.status_text(), reply.text));
    }
    parse_json(&reply.text).map_err(|e| format!("SyntaxError: {e}"))
}

pub(crate) fn copilot_headers(authorization: &str) -> Vec<(&str, &str)> {
    let mut headers = vec![("Accept", "application/json"), ("Authorization", authorization)];
    headers.extend(COPILOT_HEADERS);
    headers
}

/// `refreshGitHubCopilotAccessToken`: trades a GitHub token for a Copilot token (no model list).
pub(crate) async fn copilot_access_token(
    transport: &dyn Transport,
    github_token: &str,
    enterprise_domain: Option<&str>,
    signal: Option<&AbortSignal>,
) -> Result<OAuthCredential, String> {
    let domain = enterprise_domain.unwrap_or("github.com");
    let bearer = format!("Bearer {github_token}");
    let url = format!("https://api.{domain}/copilot_internal/v2/token");
    let raw = fetch_copilot_json(transport, "GET", &url, &copilot_headers(&bearer), String::new(), signal).await?;
    if !raw.is_object() {
        return Err("Invalid Copilot token response".into());
    }
    let (Some(token), Some(expires_at)) = (
        raw.get("token").and_then(Value::as_str),
        raw.get("expires_at").and_then(Value::as_f64),
    ) else {
        return Err("Invalid Copilot token response fields".into());
    };
    let mut extra = Map::new();
    if let Some(domain) = enterprise_domain {
        extra.insert("enterpriseUrl".into(), json!(domain));
    }
    Ok(OAuthCredential {
        refresh: github_token.to_owned(),
        access: token.to_owned(),
        expires: expires_at * 1000.0 - FIVE_MINUTES_MS,
        extra,
    })
}

async fn refresh_copilot(
    transport: &dyn Transport,
    refresh: &str,
    enterprise_domain: Option<&str>,
    signal: Option<&AbortSignal>,
) -> Result<OAuthCredential, String> {
    let mut credential = copilot_access_token(transport, refresh, enterprise_domain, signal).await?;
    // fetchGitHubCopilotModels with maxRetries 0: one request, no 429 retry.
    let base_url = copilot_base_url(Some(&credential.access), enterprise_domain);
    let bearer = format!("Bearer {}", credential.access);
    let mut headers = copilot_headers(&bearer);
    headers.push(("X-GitHub-Api-Version", COPILOT_API_VERSION));
    let models = fetch_copilot_json(
        transport,
        "GET",
        &format!("{base_url}/models"),
        &headers,
        String::new(),
        signal,
    )
    .await?;
    let catalog = CopilotCatalog::parse(&models, base_url == COPILOT_INDIVIDUAL_URL)?;
    credential
        .extra
        .insert("availableModelIds".into(), json!(catalog.available));
    Ok(credential)
}

/// `parseGitHubCopilotModelCatalog`.
pub(crate) struct CopilotCatalog {
    /// Picker-enabled, tool-capable models not disabled by policy; on the individual endpoint, policy-enabled models
    /// when no picker flag is set.
    pub available: Vec<String>,
    /// Built-in Copilot models whose policy is still unconfigured, to enable at login.
    pub policy: Vec<String>,
}

impl CopilotCatalog {
    pub fn parse(raw: &Value, allow_policy_fallback: bool) -> Result<Self, String> {
        let data = raw
            .get("data")
            .and_then(Value::as_array)
            .ok_or("Invalid Copilot models response")?;
        let models: Vec<(&str, bool, Option<&Value>)> = data
            .iter()
            .filter_map(|item| {
                let id = item.as_object()?.get("id")?.as_str()?;
                let tool_calls = item
                    .get("capabilities")
                    .and_then(|c| c.get("supports"))
                    .and_then(|s| s.get("tool_calls"));
                if tool_calls == Some(&Value::Bool(false)) {
                    return None;
                }
                let picker = item.get("model_picker_enabled") == Some(&Value::Bool(true));
                Some((id, picker, item.get("policy").and_then(|p| p.get("state"))))
            })
            .collect();
        let state_is = |state: Option<&Value>, want: &str| state.and_then(Value::as_str) == Some(want);
        let picker: Vec<String> = models
            .iter()
            .filter(|(_, picker, state)| *picker && !state_is(*state, "disabled"))
            .map(|(id, ..)| (*id).to_owned())
            .collect();
        let use_policy_fallback = allow_policy_fallback && picker.is_empty();
        let policy = models
            .iter()
            .filter(|(id, picker, state)| {
                state_is(*state, "unconfigured")
                    && crate::models::builtin_model("github-copilot", id).is_some()
                    && (*picker || use_policy_fallback)
            })
            .map(|(id, ..)| (*id).to_owned())
            .collect();
        let available = if !use_policy_fallback {
            picker
        } else {
            models
                .iter()
                .filter(|(.., state)| state_is(*state, "enabled"))
                .map(|(id, ..)| (*id).to_owned())
                .collect()
        };
        Ok(CopilotCatalog { available, policy })
    }
}
