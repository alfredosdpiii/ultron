//! Interactive OAuth logins of `auth/oauth/*.ts`: Anthropic and OpenAI Codex browser logins (PKCE, a loopback callback
//! server raced against a pasted code), the OpenAI Codex and GitHub Copilot device-code logins, and Copilot model
//! enablement. Network goes through [`Transport`]; clock, sleeping and randomness go through [`LoginEnv`] so the
//! flows replay exactly against recorded TypeScript runs (`fixtures/generate-oauth-login.mjs`, `tests/oauth_login.rs`).
//! Prompts and events are the [`crate::auth`] interaction types every OAuth flow uses.

use crate::auth::{AuthEvent, AuthInteraction, AuthPrompt, SelectOption};
use crate::http::{AbortSignal, Transport};
use crate::json::stringify;
use crate::oauth::{
    ANTHROPIC_CLIENT_ID, CODEX_CLIENT_ID, CODEX_TOKEN_URL, COPILOT_API_VERSION, COPILOT_HEADERS,
    COPILOT_INDIVIDUAL_URL, CopilotCatalog, OAuthCredential, OAuthProvider, Reply, SendError, anthropic_token_request,
    codex_credential, copilot_access_token, copilot_base_url, copilot_headers, fetch_copilot_json, normalize_domain,
    send,
};
use crate::oauth_flow::{
    CANCEL_MESSAGE as LOGIN_CANCELLED, LoginEnv, Poll, abortable_sleep, form_urlencode, generate_pkce_from,
    oauth_callback_host, oauth_error_html, oauth_success_html, poll_device_code_flow,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_CALLBACK_PORT: u16 = 53692;
const ANTHROPIC_REDIRECT_URI: &str = "http://localhost:53692/callback";
const ANTHROPIC_SCOPES: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_CALLBACK_PORT: u16 = 1455;
const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CODEX_SCOPE: &str = "openid profile email offline_access";
const CODEX_DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const CODEX_DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const CODEX_DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const CODEX_DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const CODEX_DEVICE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const MANUAL_INSTRUCTIONS: &str =
    "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here.";
const MANUAL_MESSAGE: &str = "Complete login in your browser, or paste the authorization code / redirect URL here:";

/// Everything a login needs.
pub struct LoginContext<'a> {
    pub transport: &'a dyn Transport,
    pub interaction: &'a dyn AuthInteraction,
    pub env: &'a dyn LoginEnv,
    /// Aborts the whole login.
    pub signal: AbortSignal,
    /// Interface the callback server listens on ([`oauth_callback_host`] by default).
    pub callback_host: String,
    /// Listen on this port instead of the registered one (tests); the redirect URI keeps the registered port.
    pub callback_port: Option<u16>,
}

impl<'a> LoginContext<'a> {
    pub fn new(transport: &'a dyn Transport, interaction: &'a dyn AuthInteraction, env: &'a dyn LoginEnv) -> Self {
        LoginContext {
            transport,
            interaction,
            env,
            signal: AbortSignal::new(),
            callback_host: oauth_callback_host(),
            callback_port: None,
        }
    }
}

impl OAuthProvider {
    /// Display name (`OAuthAuth.name`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (Claude Pro/Max)",
            Self::OpenAiCodex => "OpenAI (ChatGPT Plus/Pro)",
            Self::GitHubCopilot => "GitHub Copilot",
        }
    }

    /// `OAuthAuth.login`: runs the interactive login and returns the credential to store.
    pub async fn login(self, ctx: &LoginContext<'_>) -> Result<OAuthCredential, String> {
        match self {
            Self::Anthropic => login_anthropic(ctx).await,
            Self::OpenAiCodex => login_codex(ctx).await,
            Self::GitHubCopilot => login_copilot(ctx).await,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// PKCE, pasted codes, the callback server

/// What a pasted redirect URL, query string, `code#state` or bare code carries (`parseAuthorizationInput`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorizationInput {
    pub code: Option<String>,
    pub state: Option<String>,
}

pub fn parse_authorization_input(input: &str) -> AuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return AuthorizationInput::default();
    }
    let get = |url: &reqwest::Url, key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    if let Ok(url) = reqwest::Url::parse(value) {
        return AuthorizationInput {
            code: get(&url, "code"),
            state: get(&url, "state"),
        };
    }
    if value.contains('#') {
        // `value.split("#", 2)`: JS drops everything after the second piece.
        let mut parts = value.split('#');
        return AuthorizationInput {
            code: parts.next().map(str::to_owned),
            state: parts.next().map(str::to_owned),
        };
    }
    if value.contains("code=") {
        let query = value.strip_prefix('?').unwrap_or(value);
        let url = reqwest::Url::parse(&format!("http://x/?{query}")).expect("query URL");
        return AuthorizationInput {
            code: get(&url, "code"),
            state: get(&url, "state"),
        };
    }
    AuthorizationInput {
        code: Some(value.to_owned()),
        state: None,
    }
}

/// Which provider's callback route rules apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackKind {
    Anthropic,
    OpenAiCodex,
}

/// A callback server's answer to one browser request; `code` is set when it completes the login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackReply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    pub code: Option<(String, String)>,
}

const HTML: &str = "text/html; charset=utf-8";

/// The request handler of the Anthropic and Codex callback servers, for a request target such as `/callback?code=…`.
pub fn callback_reply(kind: CallbackKind, target: &str, expected_state: &str) -> CallbackReply {
    let reply = |status, body: String| CallbackReply {
        status,
        content_type: HTML,
        body,
        code: None,
    };
    let Ok(url) = reqwest::Url::parse("http://localhost").and_then(|base| base.join(target)) else {
        return match kind {
            CallbackKind::Anthropic => CallbackReply {
                status: 500,
                content_type: "text/plain; charset=utf-8",
                body: "Internal error".into(),
                code: None,
            },
            CallbackKind::OpenAiCodex => reply(
                500,
                oauth_error_html("Internal error while processing OAuth callback.", None),
            ),
        };
    };
    let param = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    let (code, state) = (param("code"), param("state"));
    match kind {
        CallbackKind::Anthropic => {
            if url.path() != "/callback" {
                return reply(404, oauth_error_html("Callback route not found.", None));
            }
            if let Some(error) = param("error").filter(|e| !e.is_empty()) {
                return reply(
                    400,
                    oauth_error_html(
                        "Anthropic authentication did not complete.",
                        Some(&format!("Error: {error}")),
                    ),
                );
            }
            let (Some(code), Some(state)) = (code.filter(|c| !c.is_empty()), state.filter(|s| !s.is_empty())) else {
                return reply(400, oauth_error_html("Missing code or state parameter.", None));
            };
            if state != expected_state {
                return reply(400, oauth_error_html("State mismatch.", None));
            }
            CallbackReply {
                code: Some((code, state)),
                ..reply(
                    200,
                    oauth_success_html("Anthropic authentication completed. You can close this window."),
                )
            }
        }
        CallbackKind::OpenAiCodex => {
            if url.path() != "/auth/callback" {
                return reply(404, oauth_error_html("Callback route not found.", None));
            }
            if state.as_deref() != Some(expected_state) {
                return reply(400, oauth_error_html("State mismatch.", None));
            }
            let Some(code) = code.filter(|c| !c.is_empty()) else {
                return reply(400, oauth_error_html("Missing authorization code.", None));
            };
            CallbackReply {
                code: Some((code, expected_state.to_owned())),
                ..reply(
                    200,
                    oauth_success_html("OpenAI authentication completed. You can close this window."),
                )
            }
        }
    }
}

/// A loopback HTTP server that answers browser redirects with [`callback_reply`] until one delivers a code. It stops
/// accepting when dropped.
pub struct CallbackServer {
    code: oneshot::Receiver<(String, String)>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl CallbackServer {
    pub async fn start(host: &str, port: u16, kind: CallbackKind, expected_state: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind((host, port)).await?;
        let (sender, code) = oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let expected_state = expected_state.to_owned();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let sender = Arc::clone(&sender);
                let expected_state = expected_state.clone();
                tokio::spawn(async move {
                    let Some(target) = read_request_target(&mut stream).await else {
                        return;
                    };
                    let reply = callback_reply(kind, &target, &expected_state);
                    let head = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        reply.status,
                        crate::oauth::status_text(reply.status),
                        reply.content_type,
                        reply.body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(reply.body.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    if let Some(code) = reply.code
                        && let Some(sender) = sender.lock().expect("callback sender").take()
                    {
                        let _ = sender.send(code);
                    }
                });
            }
        });
        Ok(CallbackServer { code, task })
    }

    /// Resolves with the first `(code, state)` a browser delivered.
    pub async fn wait_for_code(&mut self) -> Option<(String, String)> {
        (&mut self.code).await.ok()
    }
}

async fn read_request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next()?;
    line.split(' ').nth(1).map(str::to_owned)
}

/// Waits for whichever comes first: the callback server's code, the manual prompt, or the login being aborted. Then
/// mirrors upstream: a pasted answer is used when the browser delivered nothing; with no code yet, the manual prompt
/// is awaited after all. Returns `(code, state)` as given (`state` from the pasted input, if any).
async fn race_callback_and_prompt(
    ctx: &LoginContext<'_>,
    server: Option<&mut CallbackServer>,
    placeholder: &str,
    check_state: impl Fn(&AuthorizationInput) -> Result<(), String>,
) -> Result<(Option<String>, Option<String>), String> {
    let manual_signal = AbortSignal::new();
    let mut prompt = ctx.interaction.prompt(
        AuthPrompt::ManualCode {
            message: MANUAL_MESSAGE.into(),
            placeholder: Some(placeholder.into()),
        },
        Some(manual_signal.clone()),
    );
    let result = async {
        let mut manual: Option<Result<String, String>> = None;
        let from_server = match server {
            Some(server) => tokio::select! {
                code = server.wait_for_code() => code,
                answer = &mut prompt => { manual = Some(answer); None }
                () = ctx.signal.cancelled() => None,
            },
            None => {
                manual = Some((&mut prompt).await);
                None
            }
        };
        if let Some(Err(error)) = &manual {
            return Err(error.clone());
        }
        if let Some((code, state)) = from_server {
            return Ok((Some(code), Some(state)));
        }
        let parse = |input: &str| -> Result<(Option<String>, Option<String>), String> {
            let parsed = parse_authorization_input(input);
            check_state(&parsed)?;
            Ok((parsed.code, parsed.state))
        };
        let mut found = (None, None);
        if let Some(Ok(input)) = &manual
            && !input.is_empty()
        {
            found = parse(input)?;
        }
        if found.0.as_deref().is_none_or(str::is_empty) {
            let answer = match manual {
                Some(answer) => answer,
                None => (&mut prompt).await,
            };
            let input = answer?;
            if !input.is_empty() {
                found = parse(&input)?;
            }
        }
        Ok(found)
    }
    .await;
    manual_signal.abort();
    result
}

// ---------------------------------------------------------------------------------------------------------------
// Anthropic

async fn login_anthropic(ctx: &LoginContext<'_>) -> Result<OAuthCredential, String> {
    let (verifier, challenge) = generate_pkce_from(ctx.env);
    let port = ctx.callback_port.unwrap_or(ANTHROPIC_CALLBACK_PORT);
    let mut server = CallbackServer::start(&ctx.callback_host, port, CallbackKind::Anthropic, &verifier)
        .await
        .map_err(|e| format!("listen {}:{port}: {e}", ctx.callback_host))?;
    let params = form_urlencode(&[
        ("code", "true"),
        ("client_id", ANTHROPIC_CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", ANTHROPIC_REDIRECT_URI),
        ("scope", ANTHROPIC_SCOPES),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &verifier),
    ]);
    ctx.interaction.notify(AuthEvent::AuthUrl {
        url: format!("{ANTHROPIC_AUTHORIZE_URL}?{params}"),
        instructions: Some(MANUAL_INSTRUCTIONS.into()),
    });
    let (code, state) = race_callback_and_prompt(ctx, Some(&mut server), ANTHROPIC_REDIRECT_URI, |parsed| match parsed
        .state
        .as_deref()
    {
        Some(state) if !state.is_empty() && state != verifier => Err("OAuth state mismatch".into()),
        _ => Ok(()),
    })
    .await?;
    drop(server);
    let code = code.filter(|c| !c.is_empty()).ok_or("Missing authorization code")?;
    let state = state.unwrap_or_else(|| verifier.clone());
    if state.is_empty() {
        return Err("Missing OAuth state".into());
    }
    ctx.interaction.notify(AuthEvent::Progress {
        message: "Exchanging authorization code for tokens...".into(),
    });
    let body = json!({
        "grant_type": "authorization_code",
        "client_id": ANTHROPIC_CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": ANTHROPIC_REDIRECT_URI,
        "code_verifier": verifier,
    });
    anthropic_token_request(
        ctx.transport,
        &body,
        ctx.env.now_ms(),
        Some(&ctx.signal),
        |details| {
            format!(
                "Token exchange request failed. url={}; redirect_uri={ANTHROPIC_REDIRECT_URI}; response_type=authorization_code; details={details}",
                crate::oauth::ANTHROPIC_TOKEN_URL
            )
        },
        "Token exchange",
    )
    .await
}

// ---------------------------------------------------------------------------------------------------------------
// OpenAI Codex

async fn login_codex(ctx: &LoginContext<'_>) -> Result<OAuthCredential, String> {
    let option = |id: &str, label: &str| SelectOption {
        id: id.into(),
        label: label.into(),
        description: None,
    };
    let method = ctx
        .interaction
        .prompt(
            AuthPrompt::Select {
                message: "Select OpenAI Codex login method:".into(),
                options: vec![
                    option("browser", "Browser login (default)"),
                    option("device_code", "Device code login (headless)"),
                ],
            },
            None,
        )
        .await?;
    match method.as_str() {
        "device_code" => login_codex_device(ctx).await,
        "browser" => login_codex_browser(ctx).await,
        _ => Err(format!("Unknown OpenAI Codex login method: {method}")),
    }
}

/// `fetchWithLoginCancellation`: a transport failure after the login was aborted reads "Login cancelled".
async fn codex_send(
    ctx: &LoginContext<'_>,
    url: &str,
    headers: &[(&str, &str)],
    body: String,
) -> Result<Reply, String> {
    send(ctx.transport, "POST", url, headers, body, Some(&ctx.signal))
        .await
        .map_err(|e| match e {
            _ if ctx.signal.aborted() => LOGIN_CANCELLED.to_owned(),
            SendError::Aborted | SendError::Network(_) => e.message(),
        })
}

async fn codex_exchange(
    ctx: &LoginContext<'_>,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredential, String> {
    let body = form_urlencode(&[
        ("grant_type", "authorization_code"),
        ("client_id", CODEX_CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ]);
    let headers = [("Content-Type", "application/x-www-form-urlencoded")];
    let reply = codex_send(ctx, CODEX_TOKEN_URL, &headers, body).await?;
    codex_credential(&reply, "exchange", ctx.env.now_ms())
}

async fn login_codex_browser(ctx: &LoginContext<'_>) -> Result<OAuthCredential, String> {
    let (verifier, challenge) = generate_pkce_from(ctx.env);
    let mut state_bytes = [0u8; 16];
    ctx.env.fill_random(&mut state_bytes);
    let state: String = state_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let params = form_urlencode(&[
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT_ID),
        ("redirect_uri", CODEX_REDIRECT_URI),
        ("scope", CODEX_SCOPE),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "pi"),
    ]);
    let port = ctx.callback_port.unwrap_or(CODEX_CALLBACK_PORT);
    // Upstream falls back to the pasted code alone when the port is taken.
    let mut server = CallbackServer::start(&ctx.callback_host, port, CallbackKind::OpenAiCodex, &state)
        .await
        .ok();
    ctx.interaction.notify(AuthEvent::AuthUrl {
        url: format!("{CODEX_AUTHORIZE_URL}?{params}"),
        instructions: Some(MANUAL_INSTRUCTIONS.into()),
    });
    let (code, _) = race_callback_and_prompt(ctx, server.as_mut(), CODEX_REDIRECT_URI, |parsed| {
        match parsed.state.as_deref() {
            Some(s) if !s.is_empty() && s != state => Err("State mismatch".into()),
            _ => Ok(()),
        }
    })
    .await?;
    drop(server);
    let code = code.filter(|c| !c.is_empty()).ok_or("Missing authorization code")?;
    codex_exchange(ctx, &code, &verifier, CODEX_REDIRECT_URI).await
}

/// JS truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `Number(text)` for the decimal and `Infinity` forms (hex/octal/binary literals read as NaN here).
fn js_number(text: &str) -> f64 {
    let t = text.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if t.bytes().any(|b| b.is_ascii_alphabetic() && b != b'e' && b != b'E') {
        return f64::NAN;
    }
    t.parse().unwrap_or(f64::NAN)
}

async fn login_codex_device(ctx: &LoginContext<'_>) -> Result<OAuthCredential, String> {
    let headers = [("Content-Type", "application/json")];
    let body = stringify(&json!({"client_id": CODEX_CLIENT_ID}));
    let reply = codex_send(ctx, CODEX_DEVICE_USER_CODE_URL, &headers, body).await?;
    if !reply.ok() {
        if reply.status == 404 {
            return Err("OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.".into());
        }
        let suffix = if reply.text.is_empty() {
            String::new()
        } else {
            format!(": {}", reply.text)
        };
        return Err(format!(
            "OpenAI Codex device code request failed with status {}{suffix}",
            reply.status
        ));
    }
    let data = crate::json::parse_json(&reply.text).map_err(|e| format!("SyntaxError: {e}"))?;
    let interval = match data.get("interval") {
        Some(Value::String(s)) => Some(js_number(s)),
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    };
    let invalid = || format!("Invalid OpenAI Codex device code response: {}", stringify(&data));
    // ponytail: upstream also accepts non-string truthy ids/codes and passes them through; here they are invalid.
    let (Some(device_auth_id), Some(user_code), Some(interval)) = (
        data.get("device_auth_id")
            .and_then(Value::as_str)
            .filter(|_| truthy(data.get("device_auth_id"))),
        data.get("user_code")
            .and_then(Value::as_str)
            .filter(|_| truthy(data.get("user_code"))),
        interval.filter(|i| i.is_finite() && *i >= 0.0),
    ) else {
        return Err(invalid());
    };
    ctx.interaction.notify(AuthEvent::DeviceCode {
        user_code: user_code.into(),
        verification_uri: CODEX_DEVICE_VERIFICATION_URI.into(),
        interval_seconds: Some(interval),
        expires_in_seconds: Some(CODEX_DEVICE_TIMEOUT_SECONDS),
    });
    let poll_body = stringify(&json!({"device_auth_id": device_auth_id, "user_code": user_code}));
    let (code, verifier) = poll_device_code_flow(
        ctx.env,
        Some(&ctx.signal),
        Some(interval),
        Some(CODEX_DEVICE_TIMEOUT_SECONDS),
        false,
        || async {
            let reply = codex_send(ctx, CODEX_DEVICE_TOKEN_URL, &headers, poll_body.clone()).await?;
            if reply.ok() {
                let data = crate::json::parse_json(&reply.text).map_err(|e| format!("SyntaxError: {e}"))?;
                let field = |key| data.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
                return Ok(match (field("authorization_code"), field("code_verifier")) {
                    (Some(code), Some(verifier)) => Poll::Complete((code.to_owned(), verifier.to_owned())),
                    _ => Poll::Failed(format!(
                        "Invalid OpenAI Codex device auth token response: {}",
                        stringify(&data)
                    )),
                });
            }
            if matches!(reply.status, 403 | 404) {
                return Ok(Poll::Pending);
            }
            let error_code = crate::json::parse_json(&reply.text)
                .ok()
                .and_then(|json| match json.get("error") {
                    Some(Value::Object(error)) => error.get("code").and_then(Value::as_str).map(str::to_owned),
                    Some(Value::String(error)) => Some(error.clone()),
                    _ => None,
                });
            Ok(match error_code.as_deref() {
                Some("deviceauth_authorization_pending") => Poll::Pending,
                Some("slow_down") => Poll::SlowDown(None),
                _ => {
                    let suffix = if reply.text.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", reply.text)
                    };
                    Poll::Failed(format!(
                        "OpenAI Codex device auth failed with status {}{suffix}",
                        reply.status
                    ))
                }
            })
        },
    )
    .await?;
    codex_exchange(ctx, &code, &verifier, CODEX_DEVICE_REDIRECT_URI).await
}

// ---------------------------------------------------------------------------------------------------------------
// GitHub Copilot

async fn login_copilot(ctx: &LoginContext<'_>) -> Result<OAuthCredential, String> {
    let input = ctx
        .interaction
        .prompt(
            AuthPrompt::Text {
                message: "GitHub Enterprise URL/domain (blank for github.com)".into(),
                placeholder: Some("company.ghe.com".into()),
            },
            None,
        )
        .await?;
    if ctx.signal.aborted() {
        return Err(LOGIN_CANCELLED.into());
    }
    let enterprise = normalize_domain(&input);
    if !input.trim().is_empty() && enterprise.is_none() {
        return Err("Invalid GitHub Enterprise URL/domain".into());
    }
    let domain = enterprise.clone().unwrap_or_else(|| "github.com".into());
    let signal = Some(&ctx.signal);
    let form_headers = [
        ("Accept", "application/json"),
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ];

    // startDeviceFlow
    let raw = fetch_copilot_json(
        ctx.transport,
        "POST",
        &format!("https://{domain}/login/device/code"),
        &form_headers,
        form_urlencode(&[("client_id", COPILOT_CLIENT_ID), ("scope", "read:user")]),
        signal,
    )
    .await?;
    if !raw.is_object() && !raw.is_array() {
        return Err("Invalid device code response".into());
    }
    let interval = match raw.get("interval") {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(_) => Err(()),
    };
    let (Some(device_code), Some(user_code), Some(verification_uri), Ok(interval), Some(expires_in)) = (
        raw.get("device_code").and_then(Value::as_str),
        raw.get("user_code").and_then(Value::as_str),
        raw.get("verification_uri").and_then(Value::as_str),
        interval,
        raw.get("expires_in").and_then(Value::as_f64),
    ) else {
        return Err("Invalid device code response fields".into());
    };
    // The URI is opened in a browser, so only http(s) URLs are trusted.
    let verification_uri = reqwest::Url::parse(verification_uri)
        .ok()
        .filter(|url| matches!(url.scheme(), "https" | "http"))
        .ok_or("Untrusted verification_uri in device code response")?
        .to_string();
    ctx.interaction.notify(AuthEvent::DeviceCode {
        user_code: user_code.into(),
        verification_uri,
        interval_seconds: interval,
        expires_in_seconds: Some(expires_in),
    });

    // pollForGitHubAccessToken
    let poll_body = form_urlencode(&[
        ("client_id", COPILOT_CLIENT_ID),
        ("device_code", device_code),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
    ]);
    let token_url = format!("https://{domain}/login/oauth/access_token");
    let github_token = poll_device_code_flow(ctx.env, signal, interval, Some(expires_in), true, || async {
        let raw = fetch_copilot_json(
            ctx.transport,
            "POST",
            &token_url,
            &form_headers,
            poll_body.clone(),
            signal,
        )
        .await?;
        if let Some(token) = raw.get("access_token").and_then(Value::as_str) {
            return Ok(Poll::Complete(token.to_owned()));
        }
        let Some(error) = raw.get("error").and_then(Value::as_str) else {
            return Ok(Poll::Failed("Invalid device token response".into()));
        };
        Ok(match error {
            "authorization_pending" => Poll::Pending,
            "slow_down" => Poll::SlowDown(raw.get("interval").and_then(Value::as_f64)),
            _ => {
                let description = raw
                    .get("error_description")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                    .map_or(String::new(), |d| format!(": {d}"));
                Poll::Failed(format!("Device flow failed: {error}{description}"))
            }
        })
    })
    .await?;

    let mut credential = copilot_access_token(ctx.transport, &github_token, enterprise.as_deref(), signal).await?;
    let base_url = copilot_base_url(Some(&credential.access), enterprise.as_deref());
    let catalog = fetch_copilot_models(ctx, &credential.access, &base_url).await?;
    let mut available = catalog.available;
    if !catalog.policy.is_empty() {
        ctx.interaction.notify(AuthEvent::Progress {
            message: "Enabling models...".into(),
        });
        for id in enable_copilot_models(ctx, &credential.access, &base_url, &catalog.policy).await? {
            if !available.contains(&id) {
                available.push(id);
            }
        }
    }
    credential.extra.insert("availableModelIds".into(), json!(available));
    Ok(credential)
}

const LOGIN_RETRY_MAX: u32 = 2;
const LOGIN_RETRY_BUDGET_MS: f64 = 5000.0;

/// `fetchWithRateLimitRetry` with the login policy (two retries within five seconds): a 429 waits `Retry-After`
/// (seconds or an HTTP date), else 500 ms doubling, and is returned as is once the budget would be exceeded.
async fn send_with_rate_limit_retry(
    ctx: &LoginContext<'_>,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<Reply, SendError> {
    let env = ctx.env;
    let deadline = env.now_ms() + LOGIN_RETRY_BUDGET_MS;
    let mut retry = 0;
    loop {
        let reply = send(ctx.transport, method, url, headers, body.to_owned(), Some(&ctx.signal)).await?;
        if reply.status != 429 || retry == LOGIN_RETRY_MAX {
            return Ok(reply);
        }
        let mut delay = 500.0 * f64::from(2u32.pow(retry));
        if let Some(retry_after) = reply.headers.get("retry-after").filter(|v| !v.is_empty()) {
            let seconds = js_parse_float(retry_after);
            delay = if seconds.is_nan() {
                parse_http_date(retry_after).map_or(f64::NAN, |at| at - env.now_ms())
            } else {
                seconds * 1000.0
            };
            if !delay.is_finite() {
                return Ok(reply);
            }
        }
        let delay = delay.max(0.0);
        if delay >= deadline - env.now_ms() {
            return Ok(reply);
        }
        abortable_sleep(env, delay, Some(&ctx.signal), "This operation was aborted")
            .await
            .map_err(|_| SendError::Aborted)?;
        retry += 1;
    }
}

async fn fetch_copilot_models(ctx: &LoginContext<'_>, token: &str, base_url: &str) -> Result<CopilotCatalog, String> {
    let bearer = format!("Bearer {token}");
    let mut headers = copilot_headers(&bearer);
    headers.push(("X-GitHub-Api-Version", COPILOT_API_VERSION));
    let reply = send_with_rate_limit_retry(ctx, "GET", &format!("{base_url}/models"), &headers, "")
        .await
        .map_err(|e| e.message())?;
    if !reply.ok() {
        return Err(format!("{} {}: {}", reply.status, reply.status_text(), reply.text));
    }
    let raw = crate::json::parse_json(&reply.text).map_err(|e| format!("SyntaxError: {e}"))?;
    CopilotCatalog::parse(&raw, base_url == COPILOT_INDIVIDUAL_URL)
}

/// `enableGitHubCopilotModels`: best effort per model; a rate limit that outlasts the retries stops the batch.
async fn enable_copilot_models(
    ctx: &LoginContext<'_>,
    token: &str,
    base_url: &str,
    ids: &[String],
) -> Result<Vec<String>, String> {
    let bearer = format!("Bearer {token}");
    let mut headers = vec![("Content-Type", "application/json"), ("Authorization", bearer.as_str())];
    headers.extend(COPILOT_HEADERS);
    headers.push(("openai-intent", "chat-policy"));
    headers.push(("x-interaction-type", "chat-policy"));
    let mut enabled = Vec::new();
    for id in ids {
        let url = format!("{base_url}/models/{id}/policy");
        match send_with_rate_limit_retry(ctx, "POST", &url, &headers, r#"{"state":"enabled"}"#).await {
            Ok(reply) if reply.status == 429 => {
                if ctx.signal.aborted() {
                    return Err(format!("{} {}: {}", reply.status, reply.status_text(), reply.text));
                }
                break;
            }
            Ok(reply) if reply.ok() => enabled.push(id.clone()),
            Ok(_) => {}
            Err(error) if ctx.signal.aborted() => return Err(error.message()),
            Err(_) => {}
        }
    }
    Ok(enabled)
}

/// `Number.parseFloat`: the longest decimal prefix after leading whitespace, `Infinity` included; NaN without one.
fn js_parse_float(text: &str) -> f64 {
    let t = text.trim_start();
    let bytes = t.as_bytes();
    let mut i = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    if t[i..].starts_with("Infinity") {
        return if bytes.first() == Some(&b'-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let digits = |i: &mut usize| {
        let start = *i;
        while bytes.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    };
    let mut any = digits(&mut i);
    if bytes.get(i) == Some(&b'.') {
        let mut j = i + 1;
        if digits(&mut j) || any {
            any = true;
            i = j;
        }
    }
    if !any {
        return f64::NAN;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(bytes.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if digits(&mut j) {
            i = j;
        }
    }
    t[..i].trim_end_matches('.').parse().unwrap_or(f64::NAN)
}

/// `Date.parse` for the IMF-fixdate form a `Retry-After` header uses (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn parse_http_date(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = parts[..] else {
        return None;
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| *m == month)? as i64 + 1;
    let (day, year): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let hms: Vec<i64> = time.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let [h, m, s] = hms[..] else {
        return None;
    };
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + m * 60 + s) * 1000) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_number_parsing_matches_javascript() {
        assert_eq!(js_parse_float("1.5"), 1.5);
        assert_eq!(js_parse_float("  2s"), 2.0);
        assert_eq!(js_parse_float("1e3x"), 1000.0);
        assert_eq!(js_parse_float(".5"), 0.5);
        assert_eq!(js_parse_float("5."), 5.0);
        assert_eq!(js_parse_float("-Infinity"), f64::NEG_INFINITY);
        assert!(js_parse_float("Thu, 01 Jan").is_nan());
        assert!(js_parse_float(".").is_nan());
        assert_eq!(js_number(" 5 "), 5.0);
        assert_eq!(js_number(""), 0.0);
        assert!(js_number("soon").is_nan());
        assert!(js_number("5s").is_nan());
        // Date.parse("Thu, 01 Jan 2026 00:00:01 GMT"), Date.parse("Sun, 06 Nov 1994 08:49:37 GMT")
        assert_eq!(
            parse_http_date("Thu, 01 Jan 2026 00:00:01 GMT"),
            Some(1_767_225_601_000.0)
        );
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777_000.0)
        );
        assert_eq!(parse_http_date("soon"), None);
    }
}
