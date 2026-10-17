//! Replays `crates/ai/fixtures/request-auth-env.json` (TypeScript `ModelRuntime.getAuth` and the request
//! `completeSimple` sends, recorded by `crates/ai/fixtures/generate-request-auth-env.mjs`) through the session lane:
//! `ANTHROPIC_AUTH_TOKEN` as `Authorization: Bearer` (after a stored or models.json key, before the API key);
//! Cloudflare's account and gateway ids from the stored /login credential or the environment fill the base URL; the
//! AI Gateway's `cf-aig-authorization`; a stored credential's `env` reaching the request options. Requests go to an
//! in-memory transport; private temp profiles and an injected environment.

use agent::StreamFn;
use ai::http::{BoxFuture, HeaderMap};
use ai::types::{Message, TranscriptContext, UserMessage, UserMessageContent};
use ai::{Body, HttpRequest, HttpResponse, SimpleStreamOptions, Transport};
use serde_json::{Map, Value, json};
use session_core::lane::{self, Provider};
use session_core::{ModelFlags, RunEnv};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

fn fixture() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("../../ai/fixtures/request-auth-env.json")).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}

struct Case {
    spec: Value,
    env: RunEnv,
    dir: String,
    _tmp: tempfile::TempDir,
}

fn case(spec: &Value) -> Case {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().display().to_string();
    let agent = tmp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("models.json"), spec["models"].to_string()).unwrap();
    std::fs::write(agent.join("auth.json"), spec["auth"].to_string().replace("<dir>", &dir)).unwrap();
    std::fs::write(tmp.path().join("adc.json"), "{}").unwrap();
    let mut map: HashMap<String, String> = spec["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    let env = RunEnv {
        cwd: tmp.path().to_path_buf(),
        home: tmp.path().join("home"),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: tmp.path().join("docs"),
    };
    Case {
        spec: spec.clone(),
        env,
        dir,
        _tmp: tmp,
    }
}

impl Case {
    fn name(&self) -> &str {
        self.spec["name"].as_str().unwrap()
    }

    fn model(&self) -> ai::Model {
        let (provider, id) = self.spec["model"].as_str().unwrap().split_once('/').unwrap();
        lane::catalog_model(provider, id, &self.env).unwrap()
    }
}

/// What the session resolves for one request (`getAuth`'s apiKey, headers and env): the request key, and the
/// headers and env the model's per-request auth adds; `null` when the provider is not configured.
fn session_auth(case: &Case) -> Value {
    let model = case.model();
    let key = lane::request_key(&ModelFlags::default(), &case.env, &model.provider).unwrap();
    let stored = model
        .request
        .as_ref()
        .and_then(|r| r.stored_auth.as_ref())
        .and_then(|resolve| resolve());
    let headers = stored.as_ref().and_then(|s| s.headers.clone());
    let env = stored.as_ref().and_then(|s| s.env.clone());
    // The Vertex ADC marker stands for "no key, ambient credentials" (TypeScript resolves no key).
    let key = key.filter(|k| k != "<authenticated>");
    if key.is_none() && headers.is_none() && env.is_none() {
        return Value::Null;
    }
    let env: Option<BTreeMap<String, String>> = env.map(|e| e.into_iter().collect());
    let value = json!({"apiKey": key, "headers": headers, "env": env});
    serde_json::from_str(&value.to_string().replace(&case.dir, "<dir>")).unwrap()
}

#[test]
fn session_core_request_auth_env_matches_typescript_get_auth() {
    let cases = fixture();
    assert_eq!(cases.len(), 16);
    for spec in &cases {
        let case = case(spec);
        assert_eq!(session_auth(&case), spec["getAuth"], "{}", case.name());
    }
}

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// An in-memory provider endpoint that keeps each request and answers with an empty stream.
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

/// The request the lane provider sends for the case, with the key the session resolves (what every lane does).
fn send(case: &Case) -> Option<HttpRequest> {
    let model = case.model();
    let transport = Recorder::default();
    let provider = Provider::with_router(ai::claude_subscription::fallback::global(), Arc::new(transport.clone()));
    let context = TranscriptContext {
        messages: vec![Message::User(UserMessage {
            content: UserMessageContent::Text("go".into()),
            timestamp: 0,
        })],
    };
    let mut options = SimpleStreamOptions::default();
    options.base.api_key = lane::api_key(&ModelFlags::default(), &case.env, &model.provider);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    runtime.block_on(provider.stream(&model, &context, &options, &mut sink));
    transport.requests.lock().unwrap().last().cloned()
}

/// Every wire case sends to the recorded URL (placeholders filled from the resolved env) with the recorded auth
/// headers and no others among authorization, x-api-key and cf-aig-authorization.
#[test]
fn session_core_request_auth_env_requests_match_typescript() {
    let mut wired = 0;
    for spec in &fixture() {
        if spec["request"].is_null() {
            continue;
        }
        wired += 1;
        let case = case(spec);
        let request = send(&case).unwrap_or_else(|| panic!("{}: no request", case.name()));
        assert_eq!(json!(request.url), spec["request"]["url"], "{}", case.name());
        let auth: Map<String, Value> = ["authorization", "x-api-key", "cf-aig-authorization"]
            .into_iter()
            .filter_map(|name| {
                let value = request.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name))?.1;
                Some((name.to_string(), json!(value)))
            })
            .collect();
        assert_eq!(Value::Object(auth), spec["request"]["headers"], "{}", case.name());
    }
    assert_eq!(wired, 11);
}

/// Without the gateway id the AI Gateway is not configured (TypeScript: no auth), so a session has no key to send
/// and no header auth to pass its preflight.
#[test]
fn session_core_request_auth_env_gateway_without_ids_is_not_configured() {
    let spec = fixture()
        .into_iter()
        .find(|c| c["name"] == "ai gateway without a gateway id is not configured")
        .unwrap();
    assert_eq!(spec["getAuth"], Value::Null);
    let case = case(&spec);
    assert_eq!(
        lane::api_key(&ModelFlags::default(), &case.env, "cloudflare-ai-gateway"),
        None
    );
    assert!(!lane::header_auth(&case.env, "cloudflare-ai-gateway"));
}

/// `ANTHROPIC_AUTH_TOKEN` alone passes the session preflight (header auth, no key).
#[test]
fn session_core_request_auth_env_auth_token_passes_the_preflight() {
    let spec = fixture()
        .into_iter()
        .find(|c| c["name"] == "anthropic auth token wins over the api key")
        .unwrap();
    let case = case(&spec);
    assert_eq!(lane::api_key(&ModelFlags::default(), &case.env, "anthropic"), None);
    assert!(lane::header_auth(&case.env, "anthropic"));
}
