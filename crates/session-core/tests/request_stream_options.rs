//! The stream options the session's provider takes from the settings before each request (session-worker.ts
//! `providerRequestOptions`, worker-settings.ts `setStreamOptions` on `transport` / `httpIdleTimeoutMs`):
//! `retry.provider.maxRetries` / `maxRetryDelayMs` drive the SDK-style retries, `retry.provider.timeoutMs` (else the
//! idle timeout) bounds each attempt ("Request timed out.", as the SDKs' `APIConnectionTimeoutError`), a change of
//! the settings file applies to the next request, and an invalid idle timeout fails the request with
//! `parseTimeoutSetting`'s message. Option values are the TypeScript ones recorded in
//! crates/config/fixtures/worker-settings.json.

use std::sync::{Arc, Mutex};

use agent::StreamFn;
use ai::http::{BoxFuture, HeaderMap, HttpRequest, HttpResponse, Transport};
use serde_json::{Value, json};
use session_core::lane::{Provider, SettingsReader};

/// Answers every request with `status` (or never, with `None`), counting them.
struct Server {
    status: Option<u16>,
    sent: Mutex<usize>,
}

struct Empty;

impl ai::Body for Empty {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async { Ok(None) })
    }
}

impl Transport for Server {
    fn send(&self, _: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        *self.sent.lock().unwrap() += 1;
        let status = self.status;
        Box::pin(async move {
            let Some(status) = status else {
                return std::future::pending().await;
            };
            Ok(HttpResponse {
                status,
                headers: HeaderMap::new(),
                body: Box::new(Empty),
            })
        })
    }
}

fn model() -> ai::Model {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/request-settings.json")).unwrap();
    serde_json::from_value(fixture["model"].clone()).unwrap()
}

/// A profile whose settings the test rewrites, read as the session reads them.
struct Profile {
    _dir: tempfile::TempDir,
    agent: std::path::PathBuf,
    reader: SettingsReader,
}

impl Profile {
    fn new() -> Profile {
        let dir = tempfile::tempdir().unwrap();
        let (cwd, agent) = (dir.path().join("cwd"), dir.path().join("agent"));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        let read_agent = agent.clone();
        let reader: SettingsReader =
            Arc::new(move || config::settings::SettingsManager::create(&cwd, &read_agent, false));
        Profile {
            _dir: dir,
            agent,
            reader,
        }
    }
    fn set(&self, settings: Value) {
        std::fs::write(self.agent.join("settings.json"), settings.to_string()).unwrap();
    }
}

fn stream(provider: &Provider) -> ai::AssistantMessage {
    let context: ai::TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "hi", "timestamp": 1}]})).unwrap();
    let mut options = ai::SimpleStreamOptions::default();
    options.base.api_key = Some("sk-ant-api-fixture".into());
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // A hang guard, never a pass condition.
    runtime
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                provider.stream(&model(), &context, &options, &mut sink),
            )
            .await
        })
        .expect("the request never ended")
}

fn provider(server: &Arc<Server>, profile: &Profile) -> Provider {
    Provider::with_router(ai::claude_subscription::fallback::global(), server.clone())
        .with_live_settings(profile.reader.clone(), false)
}

#[test]
fn session_core_provider_retries_follow_retry_provider_settings_live() {
    let profile = Profile::new();
    let server = Arc::new(Server {
        status: Some(500),
        sent: Mutex::new(0),
    });
    let provider = provider(&server, &profile);
    // No `retry.provider.maxRetries`: one attempt (the SDKs run with maxRetries 0).
    let message = stream(&provider);
    assert_eq!(message.stop_reason, ai::StopReason::Error);
    assert_eq!(*server.sent.lock().unwrap(), 1);
    // Two retries, the next request after the file changed.
    profile.set(json!({"retry": {"provider": {"maxRetries": 2, "maxRetryDelayMs": 0}}}));
    stream(&provider);
    assert_eq!(*server.sent.lock().unwrap(), 4);
    profile.set(json!({"retry": {"provider": {"maxRetries": 0}}}));
    stream(&provider);
    assert_eq!(*server.sent.lock().unwrap(), 5);
}

#[test]
fn session_core_provider_request_timeout_follows_the_settings() {
    let profile = Profile::new();
    let server = Arc::new(Server {
        status: None,
        sent: Mutex::new(0),
    });
    let provider = provider(&server, &profile);
    // `retry.provider.timeoutMs` bounds the wait for the response; the timeout is retried like a connection error.
    profile.set(json!({"retry": {"provider": {"timeoutMs": 20, "maxRetries": 1, "maxRetryDelayMs": 0}}}));
    let message = stream(&provider);
    assert_eq!(message.stop_reason, ai::StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Request timed out."));
    assert_eq!(*server.sent.lock().unwrap(), 2);
    // Without it, the idle timeout is the request timeout.
    profile.set(json!({"httpIdleTimeoutMs": 20}));
    assert_eq!(stream(&provider).error_message.as_deref(), Some("Request timed out."));
    // An invalid idle timeout fails the request before it is sent (TS: getHttpIdleTimeoutMs throws).
    profile.set(json!({"httpIdleTimeoutMs": "soon"}));
    assert_eq!(
        stream(&provider).error_message.as_deref(),
        Some("Invalid httpIdleTimeoutMs setting: soon")
    );
    assert_eq!(*server.sent.lock().unwrap(), 3);
}

/// The options the provider reports for the session journal are TypeScript's `providerRequestOptions` for the
/// settings as they are now, plus a configured `transport` (TS `setStreamOptions({...current, transport})`).
#[test]
fn session_core_provider_stream_options_are_providerrequestoptions_of_the_current_settings() {
    let fixture: Value = serde_json::from_str(include_str!("../../config/fixtures/worker-settings.json")).unwrap();
    let profile = Profile::new();
    let server = Arc::new(Server {
        status: Some(500),
        sent: Mutex::new(0),
    });
    let provider = provider(&server, &profile);
    for case in fixture["getters"].as_array().unwrap() {
        profile.set(case["settings"].clone());
        let expected = match &case["providerRequestOptions"] {
            Value::Object(o) if o.contains_key("value") => {
                let mut options = o["value"].clone();
                if case["settings"].get("transport").is_some() || case["settings"].get("websockets").is_some() {
                    options["transport"] = case["transport"]["value"].clone();
                }
                Some(options)
            }
            _ => None,
        };
        assert_eq!(provider.stream_options(), expected, "{}", case["settings"]);
    }
}

/// `transport` fills the request's own unset transport (Codex reads it), and a request's own value wins.
#[test]
fn session_core_transport_setting_fills_the_request_transport() {
    let settings = session_core::request_settings::RequestSettings::from_settings(&json!({"transport": "websocket"}));
    let mut options = ai::SimpleStreamOptions::default();
    settings.apply(&mut options).unwrap();
    assert_eq!(options.base.transport.as_deref(), Some("websocket"));
    assert_eq!(options.base.timeout_ms, Some(300_000));
    assert_eq!(options.base.max_retry_delay_ms, Some(60_000.0));
    let mut own = ai::SimpleStreamOptions::default();
    own.base.transport = Some("sse".into());
    own.base.timeout_ms = Some(5);
    settings.apply(&mut own).unwrap();
    assert_eq!(own.base.transport.as_deref(), Some("sse"));
    assert_eq!(own.base.timeout_ms, Some(5));
    // None configured: the provider's own default ("auto" without `options.transport`, so no cached continuation).
    let mut none = ai::SimpleStreamOptions::default();
    session_core::request_settings::RequestSettings::from_settings(&json!({}))
        .apply(&mut none)
        .unwrap();
    assert_eq!(none.base.transport, None);
}
