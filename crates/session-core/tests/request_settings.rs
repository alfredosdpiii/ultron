//! The `thinkingBudgets` and `websocketConnectTimeoutMs` settings on the session's requests: replays
//! fixtures/request-settings.json (the Anthropic payload TypeScript's `streamSimple` builds with and without the
//! budgets, recorded by fixtures/generate-request-settings.mjs) through `lane::Provider` with the settings applied,
//! and checks the `options.x ?? settings.x` precedence and the invalid-timeout failure.

use std::sync::{Arc, Mutex};

use agent::StreamFn;
use ai::http::{BoxFuture, HttpRequest, HttpResponse, Transport};
use serde_json::{Value, json};
use session_core::lane::Provider;
use session_core::request_settings::RequestSettings;

#[derive(Default)]
struct Capture(Mutex<Vec<String>>);

impl Transport for Capture {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.0.lock().unwrap().push(request.body);
        Box::pin(async { Err("connection refused".to_string()) })
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/request-settings.json")).unwrap()
}

fn stream(provider: &Provider, model: &ai::Model, options: &ai::SimpleStreamOptions) -> ai::AssistantMessage {
    let context: ai::TranscriptContext = serde_json::from_value(json!({
        "messages": [{"role": "user", "content": "hi", "timestamp": 1}]
    }))
    .unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(provider.stream(model, &context, options, &mut sink))
}

#[test]
fn session_core_thinking_budgets_setting_reaches_the_request_like_ts() {
    let fixture = fixture();
    let model: ai::Model = serde_json::from_value(fixture["model"].clone()).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let capture = Arc::new(Capture::default());
        let settings = match &case["thinkingBudgets"] {
            Value::Null => json!({}),
            budgets => json!({ "thinkingBudgets": budgets }),
        };
        let provider = Provider::with_router(ai::claude_subscription::fallback::global(), capture.clone())
            .with_request_settings(RequestSettings::from_settings(&settings));
        let mut options = ai::SimpleStreamOptions::default();
        options.base.api_key = Some("sk-ant-api-fixture".into());
        options.base.max_retries = Some(0);
        options.reasoning = case["reasoning"].as_str().map(str::to_string);
        stream(&provider, &model, &options);
        let body: Value = serde_json::from_str(&capture.0.lock().unwrap()[0]).unwrap();
        assert_eq!(body["thinking"], case["thinking"], "{case}");
        assert_eq!(body["max_tokens"], case["max_tokens"], "{case}");
    }
}

/// sdk.ts `buildRequestOptions`: `options.websocketConnectTimeoutMs ?? settings`, and the Agent's budgets unless the
/// call has its own.
#[test]
fn session_core_request_settings_fill_only_what_the_request_leaves_unset() {
    let settings = RequestSettings::from_settings(&json!({
        "websocketConnectTimeoutMs": "2500",
        "thinkingBudgets": {"low": 3000},
    }));
    let mut options = ai::SimpleStreamOptions::default();
    settings.apply(&mut options).unwrap();
    assert_eq!(options.base.websocket_connect_timeout_ms, Some(2500.0));
    assert_eq!(options.thinking_budgets.and_then(|b| b.low), Some(3000));

    let mut own = ai::SimpleStreamOptions::default();
    own.base.websocket_connect_timeout_ms = Some(10.0);
    own.thinking_budgets = Some(ai::ThinkingBudgets {
        low: Some(7),
        ..Default::default()
    });
    settings.apply(&mut own).unwrap();
    assert_eq!(own.base.websocket_connect_timeout_ms, Some(10.0));
    assert_eq!(own.thinking_budgets.and_then(|b| b.low), Some(7));

    let mut none = ai::SimpleStreamOptions::default();
    RequestSettings::from_settings(&json!({})).apply(&mut none).unwrap();
    assert_eq!(none.base.websocket_connect_timeout_ms, None);
    assert_eq!(none.thinking_budgets, None);
}

/// An invalid `websocketConnectTimeoutMs` fails each request with `parseTimeoutSetting`'s message (nothing is sent);
/// a request carrying its own timeout never reads the setting.
#[test]
fn session_core_invalid_websocket_timeout_setting_fails_the_request() {
    let fixture = fixture();
    let model: ai::Model = serde_json::from_value(fixture["model"].clone()).unwrap();
    let capture = Arc::new(Capture::default());
    let provider = Provider::with_router(ai::claude_subscription::fallback::global(), capture.clone())
        .with_request_settings(RequestSettings::from_settings(
            &json!({"websocketConnectTimeoutMs": "soon"}),
        ));
    let mut options = ai::SimpleStreamOptions::default();
    options.base.api_key = Some("sk-ant-api-fixture".into());
    options.base.max_retries = Some(0);
    let message = stream(&provider, &model, &options);
    assert_eq!(message.stop_reason, ai::StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Invalid websocketConnectTimeoutMs setting: soon")
    );
    assert!(capture.0.lock().unwrap().is_empty());
    options.base.websocket_connect_timeout_ms = Some(100.0);
    stream(&provider, &model, &options);
    assert_eq!(capture.0.lock().unwrap().len(), 1);
}
