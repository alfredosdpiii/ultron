//! Provider request options the settings fill (sdk.ts `buildRequestOptions`, session-worker.ts
//! `providerRequestOptions`, worker-settings.ts `transport`, and the Agent's `thinkingBudgets`): `thinkingBudgets`
//! (the token budget each reasoning level asks for), `websocketConnectTimeoutMs` (the WebSocket transport's connect
//! timeout), the stream options of every request (`timeoutMs`: `retry.provider.timeoutMs`, else the HTTP idle
//! timeout; `retry.provider.maxRetries` and `maxRetryDelayMs`) and the `transport` setting. A request's own value
//! wins (`options.x ?? settings.x`). An invalid timeout fails the request with `Invalid <setting> setting: <value>`,
//! as TypeScript's `parseTimeoutSetting` throws while building it.

use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct RequestSettings {
    pub thinking_budgets: Option<ai::ThinkingBudgets>,
    /// The parsed timeout, or why it is invalid.
    pub websocket_connect_timeout_ms: Result<Option<f64>, String>,
    /// `providerRequestOptions` (`{timeoutMs, maxRetries?, maxRetryDelayMs}`; none: no stream defaults), or why the
    /// HTTP idle timeout is invalid.
    pub stream: Result<Option<Value>, String>,
    /// The `transport` setting when one is configured (`sse`, `websocket`, `websocket-cached`, `auto`).
    pub transport: Option<String>,
}

impl Default for RequestSettings {
    fn default() -> Self {
        RequestSettings {
            thinking_budgets: None,
            websocket_connect_timeout_ms: Ok(None),
            stream: Ok(None),
            transport: None,
        }
    }
}

impl RequestSettings {
    /// From merged settings JSON.
    pub fn from_settings(settings: &Value) -> Self {
        let Some(settings) = settings.as_object() else {
            return RequestSettings::default();
        };
        RequestSettings {
            thinking_budgets: config::settings_keys::thinking_budgets(settings)
                .and_then(|budgets| serde_json::from_value(budgets.clone()).ok()),
            websocket_connect_timeout_ms: config::settings_keys::websocket_connect_timeout_ms(settings),
            stream: config::worker_settings_keys::provider_request_options(settings).map(Some),
            transport: settings.get("transport").and_then(Value::as_str).map(str::to_string),
        }
    }

    /// From a settings manager (the worker's, read now).
    pub fn from_manager(settings: &config::settings::SettingsManager) -> Self {
        Self::from_settings(&Value::Object(settings.settings().clone()))
    }

    /// The stream options as the session's harness keeps them (an operation's `streamOptions`): `None` when the
    /// settings fill none or are invalid.
    pub fn stream_options(&self) -> Option<Value> {
        let mut options: Map<String, Value> = match &self.stream {
            Ok(Some(Value::Object(options))) => options.clone(),
            _ => return None,
        };
        if let Some(transport) = &self.transport {
            options.insert("transport".into(), transport.clone().into());
        }
        Some(Value::Object(options))
    }

    /// Fill what `options` leaves unset; `Err` when a timeout setting is invalid.
    pub fn apply(&self, options: &mut ai::SimpleStreamOptions) -> Result<(), String> {
        if options.base.websocket_connect_timeout_ms.is_none() {
            options.base.websocket_connect_timeout_ms = self.websocket_connect_timeout_ms.clone()?;
        }
        if options.thinking_budgets.is_none() {
            options.thinking_budgets = self.thinking_budgets;
        }
        if let Some(stream) = self.stream.clone()? {
            let base = &mut options.base;
            if base.timeout_ms.is_none() {
                base.timeout_ms = stream["timeoutMs"]
                    .as_f64()
                    .filter(|t| t.is_finite() && *t >= 0.0)
                    .map(|t| t.floor() as u64);
            }
            if base.max_retries.is_none() {
                base.max_retries = stream["maxRetries"].as_u64().map(|n| n.min(u64::from(u32::MAX)) as u32);
            }
            if base.max_retry_delay_ms.is_none() {
                base.max_retry_delay_ms = stream["maxRetryDelayMs"].as_f64();
            }
        }
        if options.base.transport.is_none() {
            options.base.transport = self.transport.clone();
        }
        Ok(())
    }
}
