//! Replays `crates/ai/fixtures/header-env.json` (TypeScript `ModelRuntime`, recorded by
//! `crates/ai/fixtures/generate-header-env.mjs`) through the session lane: models.json header templates of a
//! provider and of its model read the stored credential's `env` before the environment, an empty stored value
//! reading as unset. Requests go to an in-memory transport; private temp profiles and an injected environment.

use agent::StreamFn;
use ai::http::{BoxFuture, HeaderMap};
use ai::types::{Message, TranscriptContext, UserMessage, UserMessageContent};
use ai::{Body, HttpRequest, HttpResponse, SimpleStreamOptions, Transport};
use serde_json::{Map, Value, json};
use session_core::lane::{self, Provider};
use session_core::{ModelFlags, RunEnv};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

#[derive(Clone, Default)]
struct Recorder {
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for Recorder {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let mut headers = HeaderMap::new();
        headers.insert("content-type".into(), "text/event-stream".into());
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(Chunks(vec![b"data: [DONE]\n\n".to_vec()].into_iter())),
            })
        })
    }
}

#[test]
fn session_core_header_templates_see_the_stored_credential_env_like_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../../ai/fixtures/header-env.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    for spec in cases {
        let name = spec["name"].as_str().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("models.json"), fixture["models"].to_string()).unwrap();
        std::fs::write(agent.join("auth.json"), spec["auth"].to_string()).unwrap();
        let mut map: HashMap<String, String> = spec["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
        map.insert("PI_OFFLINE".into(), "1".into());
        let env = RunEnv {
            cwd: tmp.path().to_path_buf(),
            home: tmp.path().join("home"),
            var: Arc::new(move |k| map.get(k).cloned()),
            docs: tmp.path().join("docs"),
        };
        let model = lane::catalog_model("custom", "m", &env).unwrap();
        let transport = Recorder::default();
        let provider = Provider::with_router(ai::claude_subscription::fallback::global(), Arc::new(transport.clone()));
        let context = TranscriptContext {
            messages: vec![Message::User(UserMessage {
                content: UserMessageContent::Text("go".into()),
                timestamp: 0,
            })],
        };
        let mut options = SimpleStreamOptions::default();
        options.base.api_key = lane::api_key(&ModelFlags::default(), &env, &model.provider);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut sink = |_: &ai::AssistantMessageEvent| {};
        runtime.block_on(provider.stream(&model, &context, &options, &mut sink));
        let request = transport.requests.lock().unwrap().last().cloned().expect(name);
        let picked: Map<String, Value> = ["x-team", "x-both", "x-model", "authorization"]
            .into_iter()
            .filter_map(|header| {
                let value = request.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(header))?.1;
                Some((header.to_string(), json!(value)))
            })
            .collect();
        assert_eq!(Value::Object(picked), spec["request"], "{name}");
    }
}
