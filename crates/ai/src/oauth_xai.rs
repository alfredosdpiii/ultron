//! xAI OAuth device-code flow (`auth/oauth/xai.ts`).

use crate::auth::{AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use crate::http::{AbortSignal, BoxFuture, Transport, is_aborted};
use crate::oauth_flow::{
    Clock, ClockLoginEnv, Poll, fetch_text, form_request, poll_device_code_flow, trusted_http_url,
};
use serde_json::{Map, Value, json};
use std::sync::Arc;

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const REFRESH_SKEW_MS: f64 = 5.0 * 60.0 * 1000.0;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3600.0;

/// `xaiOAuth`.
pub struct XaiOAuth {
    pub transport: Arc<dyn Transport>,
    pub clock: Clock,
}

struct Response {
    ok: bool,
    status: u16,
    body: Map<String, Value>,
}

async fn post_form(
    transport: &dyn Transport,
    url: &str,
    fields: &[(&str, &str)],
    signal: Option<&AbortSignal>,
) -> Result<Response, String> {
    let request = form_request(
        url,
        fields,
        &[
            ("Accept", "application/json"),
            ("Content-Type", "application/x-www-form-urlencoded"),
        ],
    );
    let response = match fetch_text(transport, request, signal, None).await {
        Ok(response) => response,
        Err(_) if is_aborted(signal) => return Err("Login cancelled".into()),
        Err(error) => return Err(error.message()),
    };
    let body = match serde_json::from_str::<Value>(&response.text) {
        Ok(Value::Object(map)) => map,
        Ok(_) => Map::new(),
        Err(_) if is_aborted(signal) => return Err("Login cancelled".into()),
        Err(_) => return Err(format!("xAI OAuth returned invalid JSON (HTTP {})", response.status)),
    };
    Ok(Response {
        ok: response.ok(),
        status: response.status,
        body,
    })
}

fn required_string(body: &Map<String, Value>, field: &str) -> Result<String, String> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("Invalid xAI OAuth response field: {field}"))
}

fn positive_number(body: &Map<String, Value>, field: &str) -> Result<f64, String> {
    body.get(field)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .ok_or_else(|| format!("Invalid xAI OAuth response field: {field}"))
}

fn verification_uri(value: &str) -> Result<String, String> {
    trusted_http_url(Some(&json!(value)), true).ok_or_else(|| "Untrusted verification URI in xAI OAuth response".into())
}

fn request_failure(action: &str, response: &Response) -> String {
    let error = response.body.get("error").and_then(Value::as_str);
    let description = response.body.get("error_description").and_then(Value::as_str);
    let detail = [error, description]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(": ");
    format!(
        "xAI OAuth {action} failed (HTTP {}){}",
        response.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

impl XaiOAuth {
    fn credentials(
        &self,
        body: &Map<String, Value>,
        previous_refresh: Option<&str>,
    ) -> Result<OAuthCredential, String> {
        let access = required_string(body, "access_token")?;
        let refresh = match (body.get("refresh_token"), previous_refresh) {
            (None, Some(previous)) if !previous.is_empty() => previous.to_string(),
            _ => required_string(body, "refresh_token")?,
        };
        let expires_in = if body.contains_key("expires_in") {
            positive_number(body, "expires_in")?
        } else {
            DEFAULT_TOKEN_LIFETIME_SECONDS
        };
        Ok(OAuthCredential {
            refresh,
            access,
            expires: (self.clock)() as f64 + expires_in * 1000.0 - REFRESH_SKEW_MS,
            extra: Map::new(),
        })
    }
}

impl OAuthAuth for XaiOAuth {
    fn name(&self) -> &str {
        "xAI (Grok/X subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with SuperGrok or X Premium")
    }

    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let signal = interaction.signal();
            let signal = signal.as_ref();
            let transport = self.transport.as_ref();
            let response = post_form(
                transport,
                DEVICE_CODE_URL,
                &[("client_id", CLIENT_ID), ("scope", SCOPE), ("referrer", "pi")],
                signal,
            )
            .await?;
            if !response.ok {
                return Err(request_failure("device authorization", &response));
            }
            let body = &response.body;
            let interval = body
                .get("interval")
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n > 0.0);
            let complete = match body.get("verification_uri_complete").and_then(Value::as_str) {
                Some(uri) if !uri.is_empty() => Some(verification_uri(uri)?),
                _ => None,
            };
            let device_code = required_string(body, "device_code")?;
            let user_code = required_string(body, "user_code")?;
            let uri = verification_uri(&required_string(body, "verification_uri")?)?;
            let expires_in = positive_number(body, "expires_in")?;
            interaction.notify(AuthEvent::DeviceCode {
                user_code,
                verification_uri: complete.unwrap_or(uri),
                interval_seconds: interval,
                expires_in_seconds: Some(expires_in),
            });
            poll_device_code_flow(
                &ClockLoginEnv(self.clock),
                signal,
                interval,
                Some(expires_in),
                true,
                || async {
                    let response = post_form(
                        transport,
                        TOKEN_URL,
                        &[
                            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                            ("client_id", CLIENT_ID),
                            ("device_code", &device_code),
                        ],
                        signal,
                    )
                    .await?;
                    if response.ok {
                        return Ok(Poll::Complete(self.credentials(&response.body, None)?));
                    }
                    Ok(match response.body.get("error").and_then(Value::as_str) {
                        Some("authorization_pending") => Poll::Pending,
                        Some("slow_down") => Poll::SlowDown(response.body.get("interval").and_then(Value::as_f64)),
                        Some("access_denied" | "authorization_denied") => {
                            Poll::Failed("xAI device authorization was denied".into())
                        }
                        Some("expired_token") => Poll::Failed("xAI device code expired".into()),
                        _ => Poll::Failed(request_failure("device token polling", &response)),
                    })
                },
            )
            .await
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let response = post_form(
                self.transport.as_ref(),
                TOKEN_URL,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", &credential.refresh),
                ],
                signal.as_ref(),
            )
            .await?;
            if !response.ok {
                return Err(request_failure("token refresh", &response));
            }
            self.credentials(&response.body, Some(&credential.refresh))
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}
