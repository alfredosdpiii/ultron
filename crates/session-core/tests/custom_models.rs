//! models.json custom providers in the session lane: `--model`/`--provider` resolution, the available snapshot (the
//! model selector, `/model`, RPC `get_available_models`), keys (`--api-key`, the configured `$VAR`, the environment),
//! subagent models (`Config::model`), and the lane provider sending the configured headers. Private temp profile, an
//! injected environment and an in-memory transport: nothing reads the real environment or reaches the network.

use agent::StreamFn;
use ai::http::{BoxFuture, HeaderMap};
use ai::types::{Message, TranscriptContext, UserMessage, UserMessageContent};
use ai::{Body, HttpRequest, HttpResponse, SimpleStreamOptions, StopReason, Transport};
use serde_json::json;
use session_core::lane::{self, Provider};
use session_core::{ModelFlags, RunEnv};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn models_json() -> serde_json::Value {
    json!({"providers": {
        "stubproxy": {
            "baseUrl": "http://127.0.0.1:9/v1",
            "api": "openai-completions",
            "apiKey": "$STUB_KEY",
            "headers": {"x-proxy-token": "$PROXY_TOKEN"},
            "authHeader": true,
            "models": [{"id": "sol", "contextWindow": 200000, "maxTokens": 4096, "headers": {"x-model": "sol"}}, {"id": "plain"}],
            "modelOverrides": {"plain": {"maxTokens": 900}},
        },
        "deepseek": {"baseUrl": "http://127.0.0.1:9/ds", "headers": {"x-ds": "1"}},
    }})
}

fn env(dir: &Path, vars: &[(&str, &str)]) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(dir.join("cwd")).unwrap();
    std::fs::write(agent.join("models.json"), models_json().to_string()).unwrap();
    let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    RunEnv {
        cwd: dir.join("cwd"),
        home: dir.to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

fn flags(model: &str) -> ModelFlags {
    ModelFlags {
        model: Some(model.into()),
        ..ModelFlags::default()
    }
}

#[test]
fn session_core_custom_models_resolve_from_models_json() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), &[]);
    let model = lane::resolve_model(&flags("stubproxy/sol"), &env).unwrap();
    assert_eq!(
        (model.provider.as_str(), model.id.as_str(), model.api.as_str()),
        ("stubproxy", "sol", "openai-completions")
    );
    assert_eq!((model.context_window, model.max_tokens), (200_000, 4096));
    assert!(model.request.is_some(), "resolved for requests");
    // `--provider p --model p/id` tolerates the prefix; ids match case-insensitively.
    let with_provider = ModelFlags {
        provider: Some("StubProxy".into()),
        model: Some("stubproxy/PLAIN".into()),
        ..ModelFlags::default()
    };
    let plain = lane::resolve_model(&with_provider, &env).unwrap();
    assert_eq!((plain.id.as_str(), plain.max_tokens), ("plain", 900));
    // An id the provider does not list is a custom model id on the provider's default (resolveCliModel's fallback).
    let custom = lane::resolve_choice(&flags("stubproxy/nope"), &env, false).unwrap();
    assert_eq!(
        (custom.model.id.as_str(), custom.model.provider.as_str()),
        ("nope", "stubproxy")
    );
    assert_eq!(
        custom.warning.as_deref(),
        Some("Model \"nope\" not found for provider \"stubproxy\". Using custom model id.")
    );
    assert_eq!(
        lane::resolve_model(&flags("nosuch/x"), &env).unwrap_err(),
        "Model \"nosuch/x\" not found. Use --list-models to see available models."
    );
    // A stored lane configuration names the model by provider and id.
    let stored = lane::catalog_model("stubproxy", "sol", &env).unwrap();
    assert_eq!(stored.base_url, "http://127.0.0.1:9/v1");
    let deepseek = lane::catalog_model("deepseek", "deepseek-flash", &env).unwrap();
    assert_eq!(deepseek.base_url, "http://127.0.0.1:9/ds");
    // Subagent models resolve the same way; unknown ones are refused.
    let config = session_core::compose::Config::from_env(&env);
    assert_eq!(config.model("stubproxy", "sol").unwrap().id, "sol");
    assert!(config.model("stubproxy", "zzz").is_none());
}

#[test]
fn session_core_custom_models_available_and_keyed() {
    let dir = tempfile::tempdir().unwrap();
    let without = env(dir.path(), &[]);
    let ids = |env: &RunEnv| -> Vec<String> {
        lane::available_models(env)
            .into_iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect()
    };
    assert!(ids(&without).iter().all(|id| !id.starts_with("stubproxy/")));
    assert_eq!(
        lane::request_key(&ModelFlags::default(), &without, "stubproxy").unwrap_err(),
        "API key auth failed for provider stubproxy: Failed to resolve API key for provider \"stubproxy\" from environment variable: STUB_KEY"
    );
    assert_eq!(lane::api_key(&ModelFlags::default(), &without, "stubproxy"), None);
    let given = ModelFlags {
        api_key: Some("flag-key".into()),
        ..ModelFlags::default()
    };
    assert_eq!(
        lane::api_key(&given, &without, "stubproxy").as_deref(),
        Some("flag-key")
    );

    let with = env(dir.path(), &[("STUB_KEY", "k-1"), ("DEEPSEEK_API_KEY", "ds")]);
    let listed = ids(&with);
    let custom: Vec<&String> = listed.iter().filter(|id| id.starts_with("stubproxy/")).collect();
    assert_eq!(custom, ["stubproxy/sol", "stubproxy/plain"]);
    assert!(listed.contains(&"deepseek/deepseek-flash".to_string()));
    assert!(
        lane::available_models(&with)
            .iter()
            .all(|m| m.provider != "stubproxy" || m.request.is_some())
    );
    assert_eq!(
        lane::api_key(&ModelFlags::default(), &with, "stubproxy").as_deref(),
        Some("k-1")
    );
    // A built-in provider without a models.json key keeps its environment key.
    assert_eq!(
        lane::api_key(&ModelFlags::default(), &with, "deepseek").as_deref(),
        Some("ds")
    );
}

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// An in-memory chat-completions server answering "ok".
#[derive(Clone, Default)]
struct Completions {
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for Completions {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
            format!(
                "data: {}\n\n",
                json!({"id": "c", "object": "chat.completion.chunk", "created": 1, "model": "sol", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
            )
            .into_bytes()
        };
        let chunks = vec![
            chunk(json!({"role": "assistant", "content": "ok"}), serde_json::Value::Null),
            chunk(json!({}), json!("stop")),
            b"data: [DONE]\n\n".to_vec(),
        ];
        let mut headers = HeaderMap::new();
        headers.insert("content-type".into(), "text/event-stream".into());
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(Chunks(chunks.into_iter())),
            })
        })
    }
}

#[test]
fn session_core_lane_provider_sends_models_json_headers() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), &[("STUB_KEY", "k-1"), ("PROXY_TOKEN", "tok")]);
    let model = lane::resolve_model(&flags("stubproxy/sol"), &env).unwrap();
    let transport = Completions::default();
    let provider = Provider::with_router(ai::claude_subscription::fallback::global(), Arc::new(transport.clone()));
    let context = TranscriptContext {
        messages: vec![Message::User(UserMessage {
            content: UserMessageContent::Text("go".into()),
            timestamp: 0,
        })],
    };
    let mut options = SimpleStreamOptions::default();
    options.base.api_key = lane::api_key(&ModelFlags::default(), &env, "stubproxy");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let message = runtime.block_on(provider.stream(&model, &context, &options, &mut sink));
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let requests = transport.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.url, "http://127.0.0.1:9/v1/chat/completions");
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(header("authorization"), Some("Bearer k-1"));
    assert_eq!(header("x-proxy-token"), Some("tok"));
    assert_eq!(header("x-model"), Some("sol"));
    // A header that does not resolve fails the request before it is sent.
    drop(requests);
    let unresolved = env_without_token(dir.path());
    let model = lane::resolve_model(&flags("stubproxy/sol"), &unresolved).unwrap();
    let message = runtime.block_on(provider.stream(&model, &context, &options, &mut sink));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some(
            "API key auth failed for provider stubproxy: Failed to resolve provider \"stubproxy\" header \"x-proxy-token\" from environment variable: PROXY_TOKEN"
        )
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

fn env_without_token(dir: &Path) -> RunEnv {
    env(dir, &[("STUB_KEY", "k-1")])
}
