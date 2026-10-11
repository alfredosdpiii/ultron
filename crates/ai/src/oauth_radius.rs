//! Radius gateway OAuth (`auth/oauth/radius.ts`, `providers/radius-config.ts` `normalizeRadiusGatewayUrl`): a browser
//! login with PKCE and a loopback callback server (or a pasted redirect URL), or a device-code login; tokens come from
//! the gateway's `/v1/oauth/token`.

use crate::auth::{AuthEvent, AuthInteraction, AuthPrompt, ModelAuth, OAuthAuth, OAuthCredential, SelectOption};
use crate::http::{AbortSignal, BoxFuture, HttpRequest, Transport, cancelled, is_aborted};
use crate::jsutil::json_parse_error;
use crate::oauth_flow::{
    Clock, ClockLoginEnv, FetchError, Poll, TextResponse, fetch_text, form_urlencode, generate_pkce,
    oauth_callback_host, oauth_error_html, oauth_success_html, poll_device_code_flow, random_uuid,
};
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CALLBACK_PORT: u16 = 1456;
const CALLBACK_PATH: &str = "/oauth/callback";
const REDIRECT_URI: &str = "http://127.0.0.1:1456/oauth/callback";
const TOKEN_EXPIRY_SKEW_MS: f64 = 60_000.0;
const CLIENT_ID: &str = "pi-gateway";
const SCOPE: &str = "gateway offline_access";
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// `normalizeRadiusGatewayUrl`: `https://` unless a scheme is given, no trailing slashes.
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// `createRadiusOAuth`.
pub struct RadiusOAuth {
    pub name: String,
    pub gateway: String,
    pub transport: Arc<dyn Transport>,
    pub clock: Clock,
}

impl RadiusOAuth {
    pub fn new(name: &str, gateway: &str, transport: Arc<dyn Transport>, clock: Clock) -> Self {
        RadiusOAuth {
            name: name.into(),
            gateway: normalize_radius_gateway_url(gateway),
            transport,
            clock,
        }
    }

    fn url(&self, path: &str) -> String {
        reqwest::Url::parse(&self.gateway)
            .and_then(|base| base.join(path))
            .map(|u| u.to_string())
            .unwrap_or_else(|_| format!("{}{path}", self.gateway))
    }
}

/// An OAuth error response (`OAuthResponseError`).
struct OAuthResponseError {
    oauth_error: Option<String>,
    message: String,
}

enum TokenError {
    Response(OAuthResponseError),
    Other(String),
}

impl TokenError {
    fn message(self) -> String {
        match self {
            TokenError::Response(e) => e.message,
            TokenError::Other(m) => m,
        }
    }
}

fn read_oauth_error(response: &TextResponse, message: &str) -> OAuthResponseError {
    let mut oauth_error = None;
    let mut description = None;
    if !response.text.is_empty() {
        match serde_json::from_str::<Value>(&response.text) {
            Ok(data) => {
                oauth_error = data.get("error").and_then(Value::as_str).map(str::to_string);
                description = data
                    .get("error_description")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Err(_) => description = Some(response.text.clone()),
        }
    }
    let detail = match (&oauth_error, &description) {
        (Some(error), Some(description)) if !error.is_empty() && !description.is_empty() => {
            format!("{error}: {description}")
        }
        (Some(error), _) if !error.is_empty() => error.clone(),
        (_, Some(description)) if !description.is_empty() => description.clone(),
        _ => response.status.to_string(),
    };
    OAuthResponseError {
        oauth_error,
        message: format!("{message}: {detail}"),
    }
}

fn parse_json_body(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|_| json_parse_error(text).unwrap_or_else(|| "Invalid JSON".into()))
}

fn cancelled_or(error: FetchError, signal: Option<&AbortSignal>) -> String {
    if is_aborted(signal) {
        "Login cancelled".into()
    } else {
        error.message()
    }
}

impl RadiusOAuth {
    async fn request_token(
        &self,
        fields: &[(&str, &str)],
        signal: Option<&AbortSignal>,
    ) -> Result<OAuthCredential, TokenError> {
        let request = HttpRequest {
            method: "POST".into(),
            url: self.url("/v1/oauth/token"),
            headers: [
                ("accept".to_string(), "application/json".to_string()),
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            body: form_urlencode(fields),
        };
        let response = fetch_text(self.transport.as_ref(), request, signal, None)
            .await
            .map_err(|e| TokenError::Other(cancelled_or(e, signal)))?;
        if !response.ok() {
            return Err(TokenError::Response(read_oauth_error(
                &response,
                "Radius OAuth token request failed",
            )));
        }
        let data = parse_json_body(&response.text).map_err(TokenError::Other)?;
        let mut extra = Map::new();
        if let Some(scope) = data.get("scope").filter(|s| !s.is_null()) {
            extra.insert("scope".into(), scope.clone());
        }
        let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let expires_in = data.get("expires_in").and_then(Value::as_f64).unwrap_or(f64::NAN);
        Ok(OAuthCredential {
            access: text("access_token"),
            refresh: text("refresh_token"),
            expires: (self.clock)() as f64 + expires_in * 1000.0 - TOKEN_EXPIRY_SKEW_MS,
            extra,
        })
    }

    async fn login_with_device_code(&self, interaction: &dyn AuthInteraction) -> Result<OAuthCredential, String> {
        let signal = interaction.signal();
        let signal = signal.as_ref();
        let request = HttpRequest {
            method: "POST".into(),
            url: self.url("/v1/oauth/device"),
            headers: [
                ("accept".to_string(), "application/json".to_string()),
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            body: form_urlencode(&[("client_id", CLIENT_ID), ("scope", SCOPE)]),
        };
        let response = fetch_text(self.transport.as_ref(), request, signal, None)
            .await
            .map_err(|e| cancelled_or(e, signal))?;
        if !response.ok() {
            return Err(read_oauth_error(&response, "Radius OAuth device authorization failed").message);
        }
        let data = parse_json_body(&response.text)?;
        let text = |key: &str| {
            data.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let expires_in = data.get("expires_in").and_then(Value::as_f64).filter(|n| *n != 0.0);
        let (Some(device_code), Some(user_code), Some(uri), Some(expires_in)) = (
            text("device_code"),
            text("user_code"),
            text("verification_uri"),
            expires_in,
        ) else {
            return Err("Radius OAuth device authorization response is missing required fields".into());
        };
        let interval = data.get("interval").and_then(Value::as_f64);
        interaction.notify(AuthEvent::DeviceCode {
            user_code,
            verification_uri: uri,
            interval_seconds: interval,
            expires_in_seconds: Some(expires_in),
        });
        poll_device_code_flow(
            &ClockLoginEnv(self.clock),
            signal,
            interval,
            Some(expires_in),
            false,
            || async {
                match self
                    .request_token(
                        &[
                            ("grant_type", DEVICE_CODE_GRANT_TYPE),
                            ("client_id", CLIENT_ID),
                            ("device_code", &device_code),
                        ],
                        signal,
                    )
                    .await
                {
                    Ok(credential) => Ok(Poll::Complete(credential)),
                    Err(TokenError::Other(message)) => Err(message),
                    Err(TokenError::Response(error)) => match error.oauth_error.as_deref() {
                        Some("authorization_pending") => Ok(Poll::Pending),
                        Some("slow_down") => Ok(Poll::SlowDown(None)),
                        Some("expired_token") => Ok(Poll::Failed("Device authorization expired.".into())),
                        Some("access_denied") => Ok(Poll::Failed("Device authorization was denied.".into())),
                        _ => Err(error.message),
                    },
                }
            },
        )
        .await
    }

    async fn load_discovery(&self, signal: Option<&AbortSignal>) -> Result<String, String> {
        let request = HttpRequest {
            method: "GET".into(),
            url: self.url("/v1/oauth"),
            headers: [("accept".to_string(), "application/json".to_string())]
                .into_iter()
                .collect(),
            body: String::new(),
        };
        let response = fetch_text(self.transport.as_ref(), request, signal, None)
            .await
            .map_err(|e| e.message())?;
        if !response.ok() {
            return Err(format!(
                "Could not load Radius OAuth config from {}: {} {}",
                self.gateway, response.status, response.text
            ));
        }
        let discovery = parse_json_body(&response.text)?;
        discovery
            .get("authorizationEndpoint")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("Invalid Radius OAuth config from {}", self.gateway))
    }

    async fn login_with_browser(
        &self,
        endpoint: &str,
        interaction: &dyn AuthInteraction,
    ) -> Result<OAuthCredential, String> {
        let signal = interaction.signal();
        let signal = signal.as_ref();
        let (verifier, challenge) = generate_pkce();
        let state = random_uuid();
        let mut authorize = reqwest::Url::parse(endpoint).map_err(|_| format!("Invalid URL: {endpoint}"))?;
        authorize.set_query(Some(&form_urlencode(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("handoff", "url"),
            ("state", &state),
        ])));
        let server = CallbackServer::start(&state).await;
        interaction.notify(AuthEvent::Progress {
            message: format!("Listening for OAuth callback on {REDIRECT_URI}"),
        });
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize.to_string(),
            instructions: Some(
                "Continue in your browser. If the browser is on another machine, paste the final redirect URL here."
                    .into(),
            ),
        });
        let manual_abort = AbortSignal::new();
        let manual = interaction.prompt(
            AuthPrompt::ManualCode {
                message: "Complete sign-in in your browser, or paste the authorization code / redirect URL here:"
                    .into(),
                placeholder: Some(REDIRECT_URI.into()),
            },
            Some(manual_abort.clone()),
        );
        tokio::pin!(manual);
        let mut receiver = server.receiver;
        enum Outcome {
            Code(String),
            Denied,
            Manual(Result<String, String>),
            Aborted,
        }
        let outcome = tokio::select! {
            callback = &mut receiver => match callback {
                Ok(Some(code)) => Outcome::Code(code),
                Ok(None) => Outcome::Denied,
                // The port was taken: the login continues with a pasted redirect URL.
                Err(_) => Outcome::Manual(manual.as_mut().await),
            },
            input = manual.as_mut() => Outcome::Manual(input),
            () = cancelled(signal) => Outcome::Aborted,
        };
        manual_abort.abort();
        if let Some(stop) = server.stop {
            let _ = stop.send(());
        }
        let code = match outcome {
            Outcome::Code(code) => Some(code),
            Outcome::Denied | Outcome::Aborted => None,
            Outcome::Manual(input) => {
                if is_aborted(signal) {
                    None
                } else {
                    let input = input?;
                    let (code, parsed_state) = parse_authorization_input(&input);
                    if parsed_state.as_ref().is_some_and(|s| *s != state) {
                        return Err("OAuth state mismatch".into());
                    }
                    code
                }
            }
        };
        let Some(code) = code else {
            if is_aborted(signal) {
                return Err("Login cancelled".into());
            }
            return Err("OAuth callback did not complete.".into());
        };
        self.request_token(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", REDIRECT_URI),
                ("code", &code),
                ("code_verifier", &verifier),
            ],
            signal,
        )
        .await
        .map_err(TokenError::message)
    }
}

/// `parseAuthorizationInput`: a pasted redirect URL, its query string, or the bare code.
pub fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    let query_value =
        |pairs: &[(String, String)], key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    if let Ok(url) = reqwest::Url::parse(value) {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        return (query_value(&pairs, "code"), query_value(&pairs, "state"));
    }
    if value.contains("code=") {
        let query = match value.find('?') {
            Some(index) => &value[index + 1..],
            None => value,
        };
        let pairs: Vec<(String, String)> = reqwest::Url::parse(&format!("http://x/?{query}"))
            .map(|u| u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
            .unwrap_or_default();
        return (query_value(&pairs, "code"), query_value(&pairs, "state"));
    }
    (Some(value.to_string()), None)
}

/// The loopback callback server: `receiver` yields the code, `None` when the provider denied, or closes when the port
/// could not be bound.
struct CallbackServer {
    receiver: tokio::sync::oneshot::Receiver<Option<String>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl CallbackServer {
    async fn start(expected_state: &str) -> Self {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let host = oauth_callback_host();
        let Ok(listener) = tokio::net::TcpListener::bind((host.as_str(), CALLBACK_PORT)).await else {
            drop(sender);
            return CallbackServer { receiver, stop: None };
        };
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let state = expected_state.to_string();
        tokio::spawn(async move {
            let mut sender = Some(sender);
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
                let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let url = reqwest::Url::parse(REDIRECT_URI).and_then(|base| base.join(&target));
                let (status, html, result) = match url {
                    Ok(url) if url.path() == CALLBACK_PATH => {
                        let param = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
                        if param("state").as_deref() != Some(state.as_str()) {
                            (400, oauth_error_html("OAuth state mismatch.", None), None)
                        } else if let Some(error) = param("error") {
                            let message = param("error_description").unwrap_or(error);
                            (400, oauth_error_html(&message, None), Some(None))
                        } else if let Some(code) = param("code") {
                            (
                                200,
                                oauth_success_html("Signed in to Radius. You may now close this page."),
                                Some(Some(code)),
                            )
                        } else {
                            (400, oauth_error_html("Missing authorization code.", None), None)
                        }
                    }
                    _ => (404, oauth_error_html("Callback route not found.", None), None),
                };
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    _ => "Not Found",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
                    html.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                if let Some(result) = result
                    && let Some(sender) = sender.take()
                {
                    let _ = sender.send(result);
                }
            }
        });
        CallbackServer {
            receiver,
            stop: Some(stop),
        }
    }
}

impl OAuthAuth for RadiusOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let method = interaction
                .prompt(
                    AuthPrompt::Select {
                        message: format!("Sign in to {}:", self.name),
                        options: vec![
                            SelectOption {
                                id: "browser".into(),
                                label: "Sign in with browser (recommended)".into(),
                                description: None,
                            },
                            SelectOption {
                                id: "device-code".into(),
                                label: "Sign in with device code (when signing in from another device)".into(),
                                description: None,
                            },
                        ],
                    },
                    None,
                )
                .await?;
            match method.as_str() {
                "device-code" => self.login_with_device_code(interaction).await,
                "browser" => {
                    let signal = interaction.signal();
                    let endpoint = self.load_discovery(signal.as_ref()).await?;
                    self.login_with_browser(&endpoint, interaction).await
                }
                other => Err(format!("Unknown {} sign-in method: {other}", self.name)),
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            self.request_token(
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", &credential.refresh),
                ],
                signal.as_ref(),
            )
            .await
            .map_err(TokenError::message)
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}
