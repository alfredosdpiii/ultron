//! Provider request options the settings fill (sdk.ts `buildRequestOptions` and the Agent's `thinkingBudgets`):
//! `thinkingBudgets` (the token budget each reasoning level asks for) and `websocketConnectTimeoutMs` (the WebSocket
//! transport's connect timeout). A request's own value wins (`options.x ?? settings.x`). An invalid timeout fails
//! the request with `Invalid websocketConnectTimeoutMs setting: <value>`, as TypeScript's `parseTimeoutSetting`
//! throws while building it.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct RequestSettings {
    pub thinking_budgets: Option<ai::ThinkingBudgets>,
    /// The parsed timeout, or why it is invalid.
    pub websocket_connect_timeout_ms: Result<Option<f64>, String>,
}

impl Default for RequestSettings {
    fn default() -> Self {
        RequestSettings {
            thinking_budgets: None,
            websocket_connect_timeout_ms: Ok(None),
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
        }
    }

    /// Fill what `options` leaves unset; `Err` when the timeout setting is invalid.
    pub fn apply(&self, options: &mut ai::SimpleStreamOptions) -> Result<(), String> {
        if options.base.websocket_connect_timeout_ms.is_none() {
            options.base.websocket_connect_timeout_ms = self.websocket_connect_timeout_ms.clone()?;
        }
        if options.thinking_budgets.is_none() {
            options.thinking_budgets = self.thinking_budgets;
        }
        Ok(())
    }
}
