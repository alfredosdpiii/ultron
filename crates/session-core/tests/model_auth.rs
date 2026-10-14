//! Replays `crates/ai/fixtures/model-auth.json` (recorded from the TypeScript `ModelRuntime` by
//! `crates/ai/fixtures/generate-model-auth.mjs`) through the session lane: GitHub Copilot OAuth requests go to the
//! token's endpoint over a models.json `baseUrl`; `!command` keys and headers run for every request; models.json
//! Radius providers (`"oauth": "radius"`) list their catalogs, take the stored access token or `RADIUS_API_KEY`, and
//! refresh an expired token on their own gateway; Kimi For Coding OAuth authenticates by its `Authorization` header. The gateway is a loopback server in the test; requests to model
//! providers go to an in-memory transport. Private temp profiles and an injected environment.

use agent::StreamFn;
use ai::http::{BoxFuture, HeaderMap};
use ai::types::{Message, TranscriptContext, UserMessage, UserMessageContent};
use ai::{Body, HttpRequest, HttpResponse, SimpleStreamOptions, Transport};
use serde_json::{Map, Value, json};
use session_core::lane::{self, Provider};
use session_core::{ModelFlags, RunEnv};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../ai/fixtures/model-auth.json")).unwrap()
}

/// The loopback Radius gateway: answers every token request as the recording's did and keeps what it was sent.
struct Gateway {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Gateway {
    fn start() -> Gateway {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let (method, path) = (
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                );
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let form: Map<String, Value> = String::from_utf8(body)
                    .unwrap()
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(k, v)| (k.to_string(), json!(v)))
                    .collect();
                seen.lock()
                    .unwrap()
                    .push(json!({"method": method, "path": path, "body": form}));
                let reply = json!({"access_token": "refreshed-access", "refresh_token": "refreshed-refresh", "expires_in": 3600, "scope": "gateway"}).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        Gateway { port, requests }
    }
}

struct Case {
    spec: Value,
    env: RunEnv,
    dir: String,
    port: u16,
    _tmp: tempfile::TempDir,
}

impl Case {
    fn unfill(&self, text: &str) -> String {
        text.replace(&self.dir, "<dir>")
            .replace(&self.port.to_string(), "<port>")
    }

    fn name(&self) -> &str {
        self.spec["name"].as_str().unwrap()
    }
}

fn case(spec: &Value, port: u16) -> Case {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(tmp.path().join("cwd")).unwrap();
    let dir = agent.display().to_string();
    let fill = |text: String| text.replace("<dir>", &dir).replace("<port>", &port.to_string());
    std::fs::write(agent.join("models.json"), fill(spec["models"].to_string())).unwrap();
    std::fs::write(agent.join("auth.json"), spec["auth"].to_string()).unwrap();
    if !spec["store"].is_null() {
        std::fs::write(agent.join("models-store.json"), fill(spec["store"].to_string())).unwrap();
    }
    let mut map: HashMap<String, String> = spec["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), dir.clone());
    let env = RunEnv {
        cwd: tmp.path().join("cwd"),
        home: tmp.path().to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: tmp.path().join("docs"),
    };
    Case {
        spec: spec.clone(),
        env,
        dir,
        port,
        _tmp: tmp,
    }
}

fn refs(models: &[ai::Model]) -> Vec<String> {
    models.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect()
}

/// Every recorded case: catalogs, availability, and each `getAuth` in turn (key, base URL, headers), then the
/// gateway's requests and the credentials left in auth.json.
#[test]
fn session_core_model_auth_matches_typescript() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 11);
    for spec in cases {
        let gateway = Gateway::start();
        let case = case(spec, gateway.port);
        let registry = lane::registry(&case.env);
        let providers = spec["providers"].as_object().unwrap();
        for (provider, expected) in providers {
            assert_eq!(
                json!(refs(&registry.models(provider))),
                *expected,
                "{}: {provider}",
                case.name()
            );
        }
        let available: Vec<String> = refs(&lane::available_models(&case.env))
            .into_iter()
            .filter(|r| providers.keys().any(|p| r.starts_with(&format!("{p}/"))))
            .collect();
        assert_eq!(json!(available), spec["available"], "{}", case.name());
        for call in spec["calls"].as_array().unwrap() {
            let (provider, id) = call["model"].as_str().unwrap().split_once('/').unwrap();
            let model = lane::catalog_model(provider, id, &case.env).unwrap();
            assert_eq!(
                case.unfill(&model.base_url),
                call["found"]["baseUrl"].as_str().unwrap(),
                "{}",
                case.name()
            );
            for expected in call["results"].as_array().unwrap() {
                // A stored OAuth credential's base URL and headers (Kimi's bearer), resolved once per request.
                let stored = model
                    .request
                    .as_ref()
                    .and_then(|r| r.stored_auth.as_ref())
                    .and_then(|resolve| resolve());
                let stored_headers = stored.as_ref().and_then(|s| s.headers.clone());
                let actual = match lane::request_key(&ModelFlags::default(), &case.env, provider) {
                    Ok(None) if stored_headers.is_none() => Value::Null,
                    Ok(key) => {
                        let configured = match &model.request {
                            Some(request) => request.headers(provider, key.as_deref()).unwrap(),
                            None => None,
                        };
                        let configured = ai::auth::merge_headers(stored_headers.as_ref(), configured.as_ref());
                        // The model's own static headers go out with every request (`getAuth` lists them too).
                        let mut headers: Map<String, Value> = model
                            .headers
                            .iter()
                            .flatten()
                            .map(|(k, v)| (k.clone(), json!(v)))
                            .collect();
                        for (k, v) in configured.into_iter().flatten() {
                            headers.insert(k, json!(v));
                        }
                        let base_url = stored.as_ref().and_then(|s| s.base_url.clone());
                        json!({
                            "apiKey": key,
                            "baseUrl": base_url,
                            "headers": (!headers.is_empty()).then_some(Value::Object(headers)),
                        })
                    }
                    Err(error) => json!({"error": error}),
                };
                assert_eq!(
                    serde_json::from_str::<Value>(&case.unfill(&actual.to_string())).unwrap(),
                    *expected,
                    "{}",
                    case.name()
                );
            }
        }
        let requests = gateway.requests.lock().unwrap().clone();
        assert_eq!(
            serde_json::from_str::<Value>(&case.unfill(&json!(requests).to_string())).unwrap(),
            spec["gatewayRequests"],
            "{}",
            case.name()
        );
        let stored: Value =
            serde_json::from_str(&std::fs::read_to_string(Path::new(&case.dir).join("auth.json")).unwrap()).unwrap();
        let credentials: Map<String, Value> = stored
            .as_object()
            .unwrap()
            .iter()
            .map(|(p, c)| {
                (
                    p.clone(),
                    json!({"type": c["type"], "access": c["access"], "refresh": c["refresh"]}),
                )
            })
            .collect();
        assert_eq!(Value::Object(credentials), spec["credentials"], "{}", case.name());
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

/// Sends one request of `provider/id` through the lane provider with the key the session resolves for it.
fn send(case: &Case, provider: &str, id: &str, transport: &Recorder) -> HttpRequest {
    let model = lane::catalog_model(provider, id, &case.env).unwrap();
    let lane_provider = Provider::with_router(ai::claude_subscription::fallback::global(), Arc::new(transport.clone()));
    let context = TranscriptContext {
        messages: vec![Message::User(UserMessage {
            content: UserMessageContent::Text("go".into()),
            timestamp: 0,
        })],
    };
    let mut options = SimpleStreamOptions::default();
    options.base.api_key = lane::api_key(&ModelFlags::default(), &case.env, provider);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    runtime.block_on(lane_provider.stream(&model, &context, &options, &mut sink));
    transport.requests.lock().unwrap().last().cloned().unwrap()
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// A Copilot OAuth session sends its requests to the token's proxy endpoint, not the models.json `baseUrl`.
#[test]
fn session_core_copilot_oauth_requests_go_to_the_token_endpoint() {
    let fixture = fixture();
    let case = case(&fixture["cases"][0], 1);
    let transport = Recorder::default();
    let request = send(&case, "github-copilot", "gpt-5.4", &transport);
    assert!(
        request.url.starts_with("https://api.business.githubcopilot.com/"),
        "{}",
        request.url
    );
    assert_eq!(
        header(&request, "authorization"),
        Some("Bearer tid=x;exp=9999999999;proxy-ep=proxy.business.githubcopilot.com;")
    );
}

/// `!command` headers and keys run again for every request the lane sends (TypeScript resolves them uncached).
#[test]
fn session_core_command_headers_run_for_every_request() {
    let fixture = fixture();
    let case = case(&fixture["cases"][2], 1);
    let transport = Recorder::default();
    for n in 1..=3 {
        let request = send(&case, "cmdp", "m", &transport);
        assert_eq!(header(&request, "x-provider"), Some(format!("p-{n}").as_str()));
        assert_eq!(header(&request, "x-model"), Some(format!("m-{n}").as_str()));
        assert_eq!(
            header(&request, "authorization"),
            Some(format!("Bearer key-{n}").as_str())
        );
    }
}

/// A models.json Radius provider on its own gateway: an expired token is refreshed there and the request carries
/// the new one, to the stored model's base URL.
#[test]
fn session_core_radius_provider_requests_use_the_refreshed_token() {
    let fixture = fixture();
    let gateway = Gateway::start();
    let case = case(&fixture["cases"][6], gateway.port);
    let transport = Recorder::default();
    let request = send(&case, "gw", "deep", &transport);
    assert!(
        request
            .url
            .starts_with(&format!("http://127.0.0.1:{}/gw", gateway.port)),
        "{}",
        request.url
    );
    assert_eq!(header(&request, "authorization"), Some("Bearer refreshed-access"));
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
}

/// Kimi For Coding OAuth: the request carries the stored access token as `Authorization: Bearer` (its `toAuth` gives
/// a header, no key), with the models.json headers, and no `x-api-key`.
#[test]
fn session_core_kimi_oauth_requests_carry_the_bearer_header() {
    let fixture = fixture();
    let spec = &fixture["cases"][10];
    assert_eq!(spec["name"], "kimi oauth headers go out with the models.json headers");
    let case = case(spec, 1);
    let transport = Recorder::default();
    let request = send(&case, "kimi-coding", "k3", &transport);
    assert!(
        request.url.starts_with("https://api.kimi.com/coding/"),
        "{}",
        request.url
    );
    let expected = &spec["calls"][0]["results"][0]["headers"];
    assert_eq!(header(&request, "authorization"), expected["Authorization"].as_str());
    assert_eq!(header(&request, "x-kimi"), expected["x-kimi"].as_str());
    assert_eq!(header(&request, "x-api-key"), None);
}
