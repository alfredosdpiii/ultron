//! Kimi Code (subscription) OAuth device flow (`auth/oauth/kimi-coding.ts`); the token is sent as a Bearer header.

use crate::auth::{AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use crate::env::provider_env_value;
use crate::http::{AbortSignal, BoxFuture, Transport, is_aborted};
use crate::json::stringify;
use crate::oauth_flow::{
    Clock, ClockLoginEnv, Poll, TextResponse, fetch_text, form_request, poll_device_code_flow, trusted_http_url,
};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_MAX_RETRIES: u32 = 3;

/// `kimiCodingOAuth`.
pub struct KimiCodingOAuth {
    pub transport: Arc<dyn Transport>,
    pub clock: Clock,
}

/// `KIMI_CODE_OAUTH_HOST`, else `KIMI_OAUTH_HOST`, else auth.kimi.com, without trailing slashes.
pub fn oauth_host() -> String {
    provider_env_value("KIMI_CODE_OAUTH_HOST", None)
        .or_else(|| provider_env_value("KIMI_OAUTH_HOST", None))
        .unwrap_or_else(|| DEFAULT_OAUTH_HOST.into())
        .trim_end_matches('/')
        .to_string()
}

/// `readJson`: a JSON object or array, else `None`.
fn read_json(response: &TextResponse) -> Option<Value> {
    serde_json::from_str::<Value>(&response.text)
        .ok()
        .filter(|v| v.is_object() || v.is_array())
}

fn json_text(json: &Option<Value>) -> String {
    stringify(json.as_ref().unwrap_or(&Value::Null))
}

fn field<'a>(json: &'a Option<Value>, key: &str) -> Option<&'a Value> {
    json.as_ref()?.get(key)
}

fn positive(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite() && *n > 0.0)
}

async fn post(
    transport: &dyn Transport,
    url: &str,
    fields: &[(&str, &str)],
    signal: Option<&AbortSignal>,
) -> Result<TextResponse, String> {
    let request = form_request(
        url,
        fields,
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Accept", "application/json"),
        ],
    );
    fetch_text(transport, request, signal, Some(REQUEST_TIMEOUT))
        .await
        .map_err(|e| e.message())
}

impl KimiCodingOAuth {
    fn parse_token(&self, json: &Option<Value>, operation: &str) -> Result<OAuthCredential, String> {
        let access = field(json, "access_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let refresh = field(json, "refresh_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let expires_in = positive(field(json, "expires_in"));
        match (access, refresh, expires_in) {
            (Some(access), Some(refresh), Some(expires_in)) => Ok(OAuthCredential {
                refresh: refresh.into(),
                access: access.into(),
                expires: (self.clock)() as f64 + expires_in * 1000.0,
                extra: Map::new(),
            }),
            _ => Err(format!(
                "Kimi Code token {operation} response missing fields: {}",
                json_text(json)
            )),
        }
    }

    async fn refresh_token(
        &self,
        host: &str,
        refresh: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<OAuthCredential, String> {
        let mut last_error: Option<String> = None;
        for attempt in 0..=REFRESH_MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_millis(1000 * 2u64.pow(attempt - 1));
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = crate::http::cancelled(signal) => return Err("This operation was aborted".into()),
                }
            }
            if is_aborted(signal) {
                return Err("Kimi Code token refresh aborted".into());
            }
            let response = match post(
                self.transport.as_ref(),
                &format!("{host}/api/oauth/token"),
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh),
                ],
                signal,
            )
            .await
            {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
            let json = read_json(&response);
            if response.ok() {
                return self.parse_token(&json, "refresh");
            }
            let status = response.status;
            if status == 401 || status == 403 || field(&json, "error") == Some(&json!("invalid_grant")) {
                let description = field(&json, "error_description")
                    .and_then(Value::as_str)
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default();
                return Err(format!(
                    "Kimi Code token refresh unauthorized (status {status}){description}"
                ));
            }
            if (status == 429 || status >= 500) && attempt < REFRESH_MAX_RETRIES {
                last_error = Some(format!("Kimi Code token refresh failed with status {status}"));
                continue;
            }
            return Err(format!(
                "Kimi Code token refresh failed with status {status}: {}",
                json_text(&json)
            ));
        }
        Err(last_error.unwrap_or_else(|| "Kimi Code token refresh failed".into()))
    }
}

impl OAuthAuth for KimiCodingOAuth {
    fn name(&self) -> &str {
        "Kimi Code (subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Kimi Code")
    }

    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let host = oauth_host();
            let signal = interaction.signal();
            let signal = signal.as_ref();
            let transport = self.transport.as_ref();
            let response = post(
                transport,
                &format!("{host}/api/oauth/device_authorization"),
                &[("client_id", CLIENT_ID)],
                signal,
            )
            .await?;
            if !response.ok() {
                return Err(format!(
                    "Kimi Code device authorization failed with status {}{}",
                    response.status,
                    if response.text.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", response.text)
                    }
                ));
            }
            let json = read_json(&response);
            let text = |key: &str| field(&json, key).and_then(Value::as_str).map(str::to_string);
            let (Some(device_code), Some(user_code), Some(uri), Some(complete)) = (
                text("device_code"),
                text("user_code"),
                text("verification_uri"),
                text("verification_uri_complete"),
            ) else {
                return Err(format!(
                    "Invalid Kimi Code device authorization response: {}",
                    json_text(&json)
                ));
            };
            if trusted_http_url(Some(&json!(complete)), false).is_none()
                || trusted_http_url(Some(&json!(uri)), false).is_none()
            {
                return Err(format!(
                    "Invalid Kimi Code device authorization response: {}",
                    json_text(&json)
                ));
            }
            let interval = positive(field(&json, "interval")).unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS);
            let expires_in = positive(field(&json, "expires_in")).unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS);
            interaction.notify(AuthEvent::DeviceCode {
                user_code,
                verification_uri: complete,
                interval_seconds: Some(interval),
                expires_in_seconds: Some(expires_in),
            });
            let token_url = format!("{host}/api/oauth/token");
            poll_device_code_flow(
                &ClockLoginEnv(self.clock),
                signal,
                Some(interval),
                Some(expires_in),
                true,
                || async {
                    let response = post(
                        transport,
                        &token_url,
                        &[
                            ("client_id", CLIENT_ID),
                            ("device_code", &device_code),
                            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ],
                        signal,
                    )
                    .await?;
                    if response.status >= 500 {
                        return Ok(Poll::Failed(format!(
                            "Kimi Code device token request failed with status {}{}",
                            response.status,
                            if response.text.is_empty() {
                                String::new()
                            } else {
                                format!(": {}", response.text)
                            }
                        )));
                    }
                    let json = read_json(&response);
                    if response.ok() && field(&json, "access_token").is_some_and(Value::is_string) {
                        return Ok(match self.parse_token(&json, "poll") {
                            Ok(token) => Poll::Complete(token),
                            Err(message) => Poll::Failed(message),
                        });
                    }
                    let error = field(&json, "error");
                    let description = field(&json, "error_description")
                        .and_then(Value::as_str)
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default();
                    Ok(match error.and_then(Value::as_str) {
                        Some("authorization_pending") => Poll::Pending,
                        Some("slow_down") => Poll::SlowDown(positive(field(&json, "interval"))),
                        Some("expired_token") => {
                            Poll::Failed("Kimi Code device authorization expired. Please restart login.".into())
                        }
                        Some("access_denied") => Poll::Failed("Kimi Code login was denied.".into()),
                        other => Poll::Failed(format!(
                            "Kimi Code device token request failed (status {}){}",
                            response.status,
                            other.map(|e| format!(": {e}{description}")).unwrap_or_default()
                        )),
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
            self.refresh_token(&oauth_host(), &credential.refresh, signal.as_ref())
                .await
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        Ok(ModelAuth {
            headers: Some(
                [(
                    "Authorization".to_string(),
                    Some(format!("Bearer {}", credential.access)),
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        })
    }
}
