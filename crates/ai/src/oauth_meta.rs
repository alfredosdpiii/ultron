//! Meta Model API OAuth (`auth/oauth/meta.ts`): a device flow for an identity token, exchanged for a day-long Model
//! API key. The identity token is stored as `refresh` and the minted key as `access`, so a refresh re-mints the key.

use crate::auth::{AuthEvent, AuthInteraction, ModelAuth, OAuthAuth, OAuthCredential};
use crate::http::{AbortSignal, BoxFuture, HttpRequest, Transport, is_aborted};
use crate::json::stringify;
use crate::oauth_flow::{
    Clock, ClockLoginEnv, Poll, TextResponse, fetch_text, form_request, poll_device_code_flow, trusted_http_url,
};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

const CLIENT_ID: &str = "1031625952748946";
const DEVICE_AUTHORIZATION_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
const DEVICE_TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const API_KEY_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
const API_KEY_LIFETIME_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `metaOAuth`.
pub struct MetaOAuth {
    pub transport: Arc<dyn Transport>,
    pub clock: Clock,
}

fn read_json(response: &TextResponse) -> Option<Value> {
    serde_json::from_str::<Value>(&response.text)
        .ok()
        .filter(|v| v.is_object() || v.is_array())
}

fn field<'a>(json: &'a Option<Value>, key: &str) -> Option<&'a Value> {
    json.as_ref()?.get(key)
}

/// `errorDetail`: the first non-blank of `error_description`, `detail`, `message`, `error`.
fn error_detail(json: &Option<Value>) -> String {
    for key in ["error_description", "detail", "message", "error"] {
        if let Some(text) = field(json, key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            return format!(": {text}");
        }
    }
    String::new()
}

fn positive(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite() && *n > 0.0)
}

const FORM_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/x-www-form-urlencoded"),
    ("Accept", "application/json"),
];

impl MetaOAuth {
    async fn mint_api_key(&self, identity: &str, signal: Option<&AbortSignal>) -> Result<OAuthCredential, String> {
        let request = HttpRequest {
            method: "POST".into(),
            url: API_KEY_MINT_URL.into(),
            headers: [
                ("accept".to_string(), "application/json".to_string()),
                ("authorization".to_string(), format!("Bearer {identity}")),
                ("content-type".to_string(), "application/json".to_string()),
                ("x-api-version".to_string(), "1.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            body: "{}".into(),
        };
        let response = fetch_text(self.transport.as_ref(), request, signal, Some(REQUEST_TIMEOUT))
            .await
            .map_err(|e| e.message())?;
        let json = read_json(&response);
        if response.status == 401 || response.status == 403 {
            return Err(format!(
                "Meta session expired (status {}). Run `/login meta` to sign in again.{}",
                response.status,
                error_detail(&json)
            ));
        }
        if !response.ok() {
            return Err(format!(
                "Meta API key mint failed with status {}{}",
                response.status,
                error_detail(&json)
            ));
        }
        let Some(api_key) = field(&json, "api_key")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
        else {
            let action = trusted_http_url(field(&json, "action_url"), false);
            return Err(format!(
                "Meta did not issue an API key.{}",
                action
                    .map(|url| format!(" Complete setup at {url}"))
                    .unwrap_or_default()
            ));
        };
        Ok(OAuthCredential {
            refresh: identity.into(),
            access: api_key.into(),
            expires: (self.clock)() as f64 + API_KEY_LIFETIME_MS,
            extra: Map::new(),
        })
    }

    async fn login_flow(
        &self,
        interaction: &dyn AuthInteraction,
        signal: Option<&AbortSignal>,
    ) -> Result<OAuthCredential, String> {
        let transport = self.transport.as_ref();
        let response = fetch_text(
            transport,
            form_request(DEVICE_AUTHORIZATION_URL, &[("client_id", CLIENT_ID)], FORM_HEADERS),
            signal,
            Some(REQUEST_TIMEOUT),
        )
        .await
        .map_err(|e| e.message())?;
        let json = read_json(&response);
        if !response.ok() {
            return Err(format!(
                "Meta device authorization failed with status {}{}",
                response.status,
                error_detail(&json)
            ));
        }
        let device_code = field(&json, "device_code")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let user_code = field(&json, "user_code")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let uri = trusted_http_url(field(&json, "verification_uri_complete"), false)
            .or_else(|| trusted_http_url(field(&json, "verification_uri"), false));
        let (Some(device_code), Some(user_code), Some(uri)) = (device_code, user_code, uri) else {
            return Err(format!(
                "Invalid Meta device authorization response: {}",
                stringify(json.as_ref().unwrap_or(&Value::Null))
            ));
        };
        let interval = positive(field(&json, "interval"));
        let expires_in = positive(field(&json, "expires_in"));
        interaction.notify(AuthEvent::DeviceCode {
            user_code: user_code.into(),
            verification_uri: uri,
            interval_seconds: interval,
            expires_in_seconds: expires_in,
        });
        let identity = poll_device_code_flow(
            &ClockLoginEnv(self.clock),
            signal,
            interval,
            expires_in,
            true,
            || async {
                let response = fetch_text(
                    transport,
                    form_request(
                        DEVICE_TOKEN_URL,
                        &[
                            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                            ("device_code", device_code),
                            ("client_id", CLIENT_ID),
                        ],
                        FORM_HEADERS,
                    ),
                    signal,
                    Some(REQUEST_TIMEOUT),
                )
                .await
                .map_err(|e| e.message())?;
                let json = read_json(&response);
                if response.ok()
                    && let Some(token) = field(&json, "access_token")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                {
                    return Ok(Poll::Complete(token.to_string()));
                }
                Ok(match field(&json, "error").and_then(Value::as_str) {
                    Some("authorization_pending") => Poll::Pending,
                    Some("slow_down") => Poll::SlowDown(positive(field(&json, "interval"))),
                    Some("access_denied") => Poll::Failed("Meta login was denied.".into()),
                    Some("expired_token") => {
                        Poll::Failed("Meta device authorization expired. Please restart login.".into())
                    }
                    _ => Poll::Failed(format!(
                        "Meta device token request failed with status {}{}",
                        response.status,
                        error_detail(&json)
                    )),
                })
            },
        )
        .await?;
        interaction.notify(AuthEvent::Progress {
            message: "Enabling Meta Model API access...".into(),
        });
        self.mint_api_key(&identity, signal).await
    }
}

impl OAuthAuth for MetaOAuth {
    fn name(&self) -> &str {
        "Meta (Muse subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Meta")
    }

    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let signal = interaction.signal();
            let result = self.login_flow(interaction, signal.as_ref()).await;
            match result {
                Err(_) if is_aborted(signal.as_ref()) => Err("Login cancelled".into()),
                other => other,
            }
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move { self.mint_api_key(&credential.refresh, signal.as_ref()).await })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}
