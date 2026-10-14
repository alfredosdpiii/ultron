//! OpenRouter OAuth (`auth/oauth/openrouter.ts`): a PKCE login whose authorization code is exchanged for a
//! permanent, user-controlled API key. A one-shot loopback callback server on a system-chosen port races the paste
//! prompt, so a browser on another machine can still finish by pasting the redirect URL.
//!
//! Evidence: `fixtures/generate-oauth-openrouter.mjs` records the TypeScript flow; `tests/oauth_openrouter.rs` replays
//! it.

use crate::auth::{AuthEvent, AuthInteraction, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential};
use crate::http::{AbortSignal, BoxFuture, HttpRequest, Transport, cancelled, is_aborted};
use crate::oauth_flow::{
    fetch_text, form_urlencode, generate_pkce, oauth_callback_host, oauth_error_html, oauth_redirect_host,
    oauth_success_html, random_uuid,
};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const CANCELLED: &str = "Login cancelled";
/// `Number.MAX_SAFE_INTEGER`: the key never expires.
const NEVER: f64 = 9_007_199_254_740_991.0;

/// `openRouterOAuth`.
pub struct OpenRouterOAuth {
    pub transport: Arc<dyn Transport>,
}

/// `parseAuthorizationInput`: a pasted redirect URL's `code`, a query string's, or the bare value.
fn parse_authorization_input(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    let code_of = |url: &reqwest::Url| {
        url.query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())
    };
    if let Ok(url) = reqwest::Url::parse(value) {
        return code_of(&url);
    }
    if value.contains("code=") {
        return reqwest::Url::parse(&format!("http://x/?{value}"))
            .ok()
            .and_then(|url| code_of(&url));
    }
    Some(value.to_string())
}

/// `errorDetail`.
fn error_detail(body: &Map<String, Value>) -> Option<String> {
    for key in ["error_description", "message", "error"] {
        if let Some(text) = body.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    body.get("error")
        .and_then(Value::as_object)
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `exchangeAuthorizationCode`: the code and verifier for a key.
async fn exchange(
    transport: &dyn Transport,
    code: &str,
    verifier: &str,
    signal: Option<&AbortSignal>,
) -> Result<OAuthCredential, String> {
    if is_aborted(signal) {
        return Err(CANCELLED.into());
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: TOKEN_URL.into(),
        headers: [
            ("accept".to_string(), "application/json".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]
        .into_iter()
        .collect(),
        body: json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"}).to_string(),
    };
    let response = match fetch_text(transport, request, signal, Some(TOKEN_EXCHANGE_TIMEOUT)).await {
        Ok(response) => response,
        Err(_) if is_aborted(signal) => return Err(CANCELLED.into()),
        Err(crate::oauth_flow::FetchError::Timeout) => {
            return Err("OpenRouter OAuth token exchange timed out".into());
        }
        Err(error) => return Err(error.message()),
    };
    let body = match serde_json::from_str::<Value>(&response.text) {
        Ok(Value::Object(body)) => body,
        Ok(_) => Map::new(),
        Err(_) if response.ok() => return Err("OpenRouter OAuth returned invalid JSON".into()),
        Err(_) => Map::new(),
    };
    if !response.ok() {
        let detail = error_detail(&body).map(|d| format!(": {d}")).unwrap_or_default();
        return Err(format!(
            "OpenRouter OAuth key exchange failed (HTTP {}){detail}",
            response.status
        ));
    }
    let Some(key) = body.get("key").and_then(Value::as_str).filter(|k| !k.is_empty()) else {
        return Err("OpenRouter OAuth response carries no \"key\"".into());
    };
    Ok(OAuthCredential {
        access: key.to_string(),
        refresh: String::new(),
        expires: NEVER,
        extra: Map::new(),
    })
}

/// What the callback server settled the login with: a credential, `None` (handed over to the paste prompt), or an
/// error.
type Settled = Result<Option<OAuthCredential>, String>;

#[derive(Default)]
struct CallbackState {
    claimed: bool,
    settled: bool,
    sender: Option<tokio::sync::oneshot::Sender<Settled>>,
}

impl CallbackState {
    fn finish(&mut self, result: Settled) {
        if self.settled {
            return;
        }
        self.settled = true;
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result);
        }
    }
}

/// The one-shot loopback callback server (`startCallbackServer`).
struct CallbackServer {
    url: String,
    state: Arc<Mutex<CallbackState>>,
    receiver: tokio::sync::oneshot::Receiver<Settled>,
    stop: tokio::sync::oneshot::Sender<()>,
}

impl CallbackServer {
    async fn start(
        path: String,
        verifier: String,
        transport: Arc<dyn Transport>,
        signal: Option<AbortSignal>,
    ) -> Result<Self, String> {
        let host = oauth_callback_host();
        let listener = tokio::net::TcpListener::bind((host.as_str(), 0))
            .await
            .map_err(|e| e.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|_| "Could not determine the OpenRouter OAuth callback port".to_string())?
            .port();
        let url = format!("http://{}:{port}{path}", oauth_redirect_host(&host));
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let state = Arc::new(Mutex::new(CallbackState {
            sender: Some(sender),
            ..CallbackState::default()
        }));
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let shared = state.clone();
        let base = url.clone();
        tokio::spawn(async move {
            let lock = |s: &Arc<Mutex<CallbackState>>| s.lock().unwrap_or_else(|e| e.into_inner()).settled;
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    _ = &mut stopped => return,
                };
                let Ok((mut socket, _)) = accepted else { continue };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buffer).into_owned();
                let mut parts = head.split_whitespace();
                let (method, target) = (parts.next().unwrap_or("GET"), parts.next().unwrap_or("/"));
                let url = reqwest::Url::parse(&base).and_then(|b| b.join(target));
                let expected = reqwest::Url::parse(&base)
                    .map(|b| b.path().to_string())
                    .unwrap_or_default();
                let (status, html) = match url {
                    Ok(url) if method == "GET" && url.path() == expected => {
                        let param = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
                        let claimed = shared.lock().unwrap_or_else(|e| e.into_inner()).claimed;
                        if claimed || lock(&shared) {
                            (
                                409,
                                oauth_error_html("This OAuth callback has already been used.", None),
                            )
                        } else if let Some(error) = param("error") {
                            let description = param("error_description").unwrap_or(error);
                            shared
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .finish(Err(format!("OpenRouter authorization failed: {description}")));
                            (
                                400,
                                oauth_error_html("OpenRouter authorization was denied.", Some(&description)),
                            )
                        } else if let Some(code) = param("code") {
                            shared.lock().unwrap_or_else(|e| e.into_inner()).claimed = true;
                            match exchange(transport.as_ref(), &code, &verifier, signal.as_ref()).await {
                                Ok(credential) => {
                                    shared
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .finish(Ok(Some(credential)));
                                    (
                                        200,
                                        oauth_success_html("Signed in to OpenRouter. You may now close this page."),
                                    )
                                }
                                Err(error) => {
                                    let page = oauth_error_html("OpenRouter key exchange failed.", Some(&error));
                                    shared.lock().unwrap_or_else(|e| e.into_inner()).finish(Err(error));
                                    (502, page)
                                }
                            }
                        } else {
                            (
                                400,
                                oauth_error_html("OpenRouter returned no authorization code.", None),
                            )
                        }
                    }
                    _ => (404, oauth_error_html("OAuth callback route not found.", None)),
                };
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    404 => "Not Found",
                    409 => "Conflict",
                    _ => "Bad Gateway",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
                    html.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                if lock(&shared) {
                    return;
                }
            }
        });
        Ok(CallbackServer {
            url,
            state,
            receiver,
            stop,
        })
    }
}

impl OpenRouterOAuth {
    async fn login_flow(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        let signal = interaction.signal();
        let (verifier, challenge) = generate_pkce();
        let path = format!("/oauth/callback/{}", random_uuid());
        if is_aborted(signal.as_ref()) {
            return Err(CANCELLED.into());
        }
        let server = CallbackServer::start(path, verifier.clone(), self.transport.clone(), signal.clone()).await?;
        let callback = server.url.clone();
        let mut authorize = reqwest::Url::parse(AUTHORIZE_URL).map_err(|e| e.to_string())?;
        authorize.set_query(Some(&form_urlencode(&[
            ("callback_url", &callback),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])));
        interaction.notify(AuthEvent::Progress {
            message: format!("Listening for OpenRouter OAuth callback on {callback}"),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize.to_string(),
            instructions: Some(
                "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .into(),
            ),
        });
        let manual_abort = AbortSignal::new();
        let manual = interaction.prompt(
            AuthPrompt::ManualCode {
                message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                    .into(),
                placeholder: Some(callback.clone()),
            },
            Some(manual_abort.clone()),
        );
        tokio::pin!(manual);
        let CallbackServer {
            state,
            mut receiver,
            stop,
            ..
        } = server;
        let cancel_wait = || {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.claimed {
                state.finish(Ok(None));
            }
        };
        let mut manual_result: Option<Result<String, String>> = None;
        let settled = tokio::select! {
            settled = &mut receiver => settled.unwrap_or(Ok(None)),
            input = manual.as_mut() => {
                manual_result = Some(input);
                cancel_wait();
                (&mut receiver).await.unwrap_or(Ok(None))
            }
            () = cancelled(signal.as_ref()) => Err(CANCELLED.into()),
            () = tokio::time::sleep(LOGIN_TIMEOUT) => Err("OpenRouter OAuth login timed out".into()),
        };
        let outcome = async {
            if let Some(Err(error)) = &manual_result {
                return Err(error.clone());
            }
            if let Some(credential) = settled? {
                return Ok(credential);
            }
            let input = match manual_result.take() {
                Some(input) => input?,
                None => manual.as_mut().await?,
            };
            let Some(code) = parse_authorization_input(&input) else {
                return Err("Missing authorization code".into());
            };
            interaction.notify(AuthEvent::Progress {
                message: "Exchanging authorization code for an API key...".into(),
            });
            exchange(self.transport.as_ref(), &code, &verifier, signal.as_ref()).await
        }
        .await;
        manual_abort.abort();
        let _ = stop.send(());
        outcome
    }
}

impl OAuthAuth for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter OAuth"
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with OpenRouter")
    }

    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(self.login_flow(interaction))
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        _signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move { Ok(credential.clone()) })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}
