//! Replays `crates/ai/fixtures/provider-headers.json` (TypeScript `ModelRuntime.completeSimple`, recorded by
//! `crates/ai/fixtures/generate-provider-headers.mjs`) through the session lane: OpenCode Zen and OpenCode Go send
//! `x-opencode-session: <sessionId>` on every API unless the caller set it; other providers do not. The cases that
//! set `OPENAI_ORG_ID`/`OPENAI_PROJECT_ID` (the OpenAI SDK reads the process environment) replay through the binary
//! in `crates/cli/tests/provider_wire_auth.rs`. Requests go to an in-memory transport; private temp profiles.

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

fn run_env(dir: &std::path::Path, vars: &Map<String, Value>) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let mut map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    RunEnv {
        cwd: dir.to_path_buf(),
        home: dir.join("home"),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

#[test]
fn session_core_opencode_session_header_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../../ai/fixtures/provider-headers.json")).unwrap();
    let mut replayed = 0;
    for spec in fixture["cases"].as_array().unwrap() {
        let name = spec["name"].as_str().unwrap();
        let vars = spec["env"].as_object().unwrap();
        if vars.contains_key("OPENAI_ORG_ID") || vars.contains_key("OPENAI_PROJECT_ID") {
            continue;
        }
        replayed += 1;
        let dir = tempfile::tempdir().unwrap();
        let env = run_env(dir.path(), vars);
        let (provider, id) = spec["model"].as_str().unwrap().split_once('/').unwrap();
        let model = lane::catalog_model(provider, id, &env).unwrap();
        let transport = Recorder::default();
        let lane_provider =
            Provider::with_router(ai::claude_subscription::fallback::global(), Arc::new(transport.clone()));
        let context = TranscriptContext {
            messages: vec![Message::User(UserMessage {
                content: UserMessageContent::Text("go".into()),
                timestamp: 0,
            })],
        };
        let mut options = SimpleStreamOptions::default();
        options.base.api_key = lane::api_key(&ModelFlags::default(), &env, provider);
        options.base.session_id = spec["options"]["sessionId"].as_str().map(str::to_string);
        options.base.headers = spec["options"]["headers"].as_object().map(|h| {
            h.iter()
                .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string)))
                .collect()
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut sink = |_: &ai::AssistantMessageEvent| {};
        runtime.block_on(lane_provider.stream(&model, &context, &options, &mut sink));
        let request = transport.requests.lock().unwrap().last().cloned().unwrap();
        assert_eq!(json!(request.url), spec["request"]["url"], "{name}");
        let seen: Map<String, Value> = ["x-opencode-session", "openai-organization", "openai-project"]
            .into_iter()
            .filter_map(|n| {
                let value = request.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n))?.1;
                Some((n.to_string(), json!(value)))
            })
            .collect();
        assert_eq!(Value::Object(seen), spec["request"]["headers"], "{name}");
    }
    assert_eq!(replayed, 10);
}
