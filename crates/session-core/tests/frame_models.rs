//! A frame naming another model (`rlm.infer(model=...)`, the frame model settings) calls it as the session's lane of
//! that model would (TS: the frame task lane's `setModel`, then the registry's model and the provider's `getAuth`
//! key): `session_core::frame_models::resolver` resolves the built-in catalog and models.json, and the request goes
//! out with the models.json headers and key. Private temp profile, injected environment, in-memory transport.

use ai::http::{BoxFuture, HeaderMap};
use ai::{Body, HttpRequest, HttpResponse, Transport};
use inference::Inference;
use kernel::{EchoHandler, HostHandler, Signal};
use serde_json::{Value, json};
use session_core::compose::Config;
use session_core::{ModelFlags, RunEnv, lane};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn env(dir: &Path, vars: &[(&str, &str)]) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(dir.join("cwd")).unwrap();
    let models = json!({"providers": {"stubproxy": {
        "baseUrl": "http://127.0.0.1:9/v1", "api": "openai-completions", "apiKey": "$STUB_KEY",
        "headers": {"x-proxy-token": "$PROXY_TOKEN"}, "authHeader": true,
        "models": [{"id": "sol", "contextWindow": 200000, "maxTokens": 4096, "headers": {"x-model": "sol"}}, {"id": "plain"}],
    }}});
    std::fs::write(agent.join("models.json"), models.to_string()).unwrap();
    let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    RunEnv {
        cwd: dir.join("cwd"),
        home: dir.to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// An in-memory chat-completions server answering "ok"; keeps every request.
#[derive(Clone, Default)]
struct Completions {
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for Completions {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let chunk = |delta: Value, finish: Value| {
            format!(
                "data: {}\n\n",
                json!({"id": "c", "object": "chat.completion.chunk", "created": 1, "model": "sol", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
            )
            .into_bytes()
        };
        let chunks = vec![
            chunk(json!({"role": "assistant", "content": "ok"}), Value::Null),
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
fn session_core_frame_models_resolve_the_catalog_and_models_json_with_the_providers_key() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), &[("STUB_KEY", "k-1"), ("DEEPSEEK_API_KEY", "ds")]);
    let resolve = session_core::frame_models::resolver(Arc::new(Config::from_env(&env)));
    let (model, key) = resolve("stubproxy/sol").unwrap();
    assert_eq!(
        (model.provider.as_str(), model.id.as_str(), model.max_tokens),
        ("stubproxy", "sol", 4096)
    );
    assert!(model.request.is_some(), "resolved for requests");
    assert_eq!(key.as_deref(), Some("k-1"));
    let (model, key) = resolve("deepseek/deepseek-flash").unwrap();
    assert_eq!(model.api, "openai-completions");
    assert_eq!(key.as_deref(), Some("ds"));
    assert_eq!(resolve("nosuch/x").unwrap_err(), "Model \"nosuch/x\" not found");
}

#[test]
fn session_core_frame_naming_a_models_json_model_sends_its_headers_and_key() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), &[("STUB_KEY", "k-1"), ("PROXY_TOKEN", "tok")]);
    let own = lane::catalog_model("stubproxy", "plain", &env).unwrap();
    let transport = Completions::default();
    let handler = Inference::new(
        Arc::new(EchoHandler),
        own,
        lane::api_key(&ModelFlags::default(), &env, "stubproxy"),
        Arc::new(transport.clone()),
        dir.path().join("context"),
    )
    .with_models(session_core::frame_models::resolver(Arc::new(Config::from_env(&env))));
    let Value::Object(payload) = json!({"task": "t", "model": "stubproxy/sol"}) else {
        unreachable!()
    };
    let observed = handler.handle("rlm.infer", payload, &Signal::new()).unwrap();
    assert_eq!(observed["value"], "ok", "{observed}");
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
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], "sol");
}
