//! Replays `fixtures/oauth-login.json` (recorded from the TypeScript login flows by `fixtures/generate-oauth-login.mjs`):
//! the same scripted prompt answers, canned responses, fake clock and counter randomness must yield the same prompts,
//! events, requests (and when they were sent), callback-server answers and credential or error.

use ai::auth::{AuthEvent, AuthInteraction, AuthPrompt};
use ai::http::{AbortSignal, Body, BoxFuture, HeaderMap, HttpRequest, HttpResponse, Transport};
use ai::oauth::OAuthProvider;
use ai::oauth_flow::{LoginEnv, generate_pkce_from, oauth_error_html, oauth_success_html};
use ai::oauth_login::{CallbackKind, LoginContext, callback_reply, parse_authorization_input};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const START: f64 = 1_767_225_600_000.0;

struct OnceBody(Option<Vec<u8>>);

impl Body for OnceBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

struct FakeEnv {
    now: Arc<Mutex<f64>>,
    counter: Mutex<u8>,
}

impl LoginEnv for FakeEnv {
    fn now_ms(&self) -> f64 {
        *self.now.lock().unwrap()
    }

    fn sleep(&self, ms: f64) -> BoxFuture<'_, ()> {
        *self.now.lock().unwrap() += ms.max(0.0);
        Box::pin(async {})
    }

    fn fill_random(&self, buf: &mut [u8]) {
        let mut counter = self.counter.lock().unwrap();
        for b in buf {
            *b = *counter;
            *counter = counter.wrapping_add(1);
        }
    }
}

struct Mock {
    queue: Mutex<Vec<(u16, String, HeaderMap)>>,
    requests: Mutex<Vec<Value>>,
    now: Arc<Mutex<f64>>,
    abort_at: usize,
    signal: AbortSignal,
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        let at = *self.now.lock().unwrap() as u64;
        let count = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(json!({"at": at, "url": request.url, "method": request.method, "headers": request.headers, "body": request.body}));
            requests.len()
        };
        if count == self.abort_at {
            self.signal.abort();
        }
        let next = {
            let mut queue = self.queue.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        Box::pin(async move {
            let (status, body, headers) = next.ok_or("unexpected request")?;
            if status == 0 {
                return Err("fetch failed".into());
            }
            Ok(HttpResponse {
                status,
                headers,
                body: Box::new(OnceBody(Some(body.into_bytes()))),
            })
        })
    }
}

#[derive(Default)]
struct Recorded {
    prompts: Vec<Value>,
    events: Vec<Value>,
    state: String,
    redirect: String,
    hits: Option<tokio::task::JoinHandle<Vec<Value>>>,
}

struct Script {
    answers: Mutex<Vec<Value>>,
    recorded: Mutex<Recorded>,
    hits: Vec<String>,
    port: u16,
}

fn fill(text: &str, state: &str, redirect: &str) -> String {
    text.replace("{state}", state).replace("{redirect}", redirect)
}

impl AuthInteraction for Script {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>> {
        let mut recorded = self.recorded.lock().unwrap();
        let mut json = prompt.to_json();
        json["hasSignal"] = json!(cancel.is_some());
        recorded.prompts.push(json);
        let answer = {
            let mut answers = self.answers.lock().unwrap();
            (!answers.is_empty()).then(|| answers.remove(0))
        };
        let (state, redirect) = (recorded.state.clone(), recorded.redirect.clone());
        Box::pin(async move {
            match answer {
                None => Err("unexpected prompt".into()),
                Some(Value::String(text)) => Ok(fill(&text, &state, &redirect)),
                Some(answer) if answer.get("reject").is_some() => Err(answer["reject"].as_str().unwrap().to_owned()),
                Some(_) => {
                    cancel.expect("waiting prompt has a signal").cancelled().await;
                    Err("Login cancelled".into())
                }
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        let mut recorded = self.recorded.lock().unwrap();
        recorded.events.push(event.to_json());
        let AuthEvent::AuthUrl { url, .. } = event else {
            return;
        };
        let url = reqwest::Url::parse(&url).unwrap();
        let param = |key: &str| {
            url.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        recorded.state = param("state");
        recorded.redirect = param("redirect_uri");
        if self.hits.is_empty() {
            return;
        }
        let paths: Vec<(String, String)> = self
            .hits
            .iter()
            .map(|p| (p.clone(), fill(p, &recorded.state, &recorded.redirect)))
            .collect();
        let port = self.port;
        recorded.hits = Some(tokio::spawn(async move {
            let client = reqwest::Client::new();
            let mut out = Vec::new();
            for (path, filled) in paths {
                let response = client
                    .get(format!("http://127.0.0.1:{port}{filled}"))
                    .send()
                    .await
                    .unwrap();
                let status = response.status().as_u16();
                let content_type = response.headers()["content-type"].to_str().unwrap().to_owned();
                let body = response.text().await.unwrap();
                out.push(json!({"path": path, "status": status, "contentType": content_type, "body": body}));
            }
            out
        }));
    }
}

/// TS appends `; stack=<node stack>` to Anthropic error details; the stack and V8's JSON.parse wording are not ported.
fn comparable(message: &str) -> String {
    let message = message.split("; stack=").next().unwrap_or_default();
    match message.find("details=SyntaxError: ") {
        Some(i) => message[..i + "details=SyntaxError: ".len()].to_owned(),
        None => message.to_owned(),
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/oauth-login.json")).unwrap()
}

async fn replay(case: &Value) {
    let name = case["name"].as_str().unwrap();
    let provider = OAuthProvider::from_id(case["provider"].as_str().unwrap()).unwrap();
    let now = Arc::new(Mutex::new(START));
    let signal = AbortSignal::new();
    let queue = case["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let headers: HeaderMap = r
                .get(2)
                .and_then(Value::as_object)
                .map(|h| {
                    h.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            (
                r[0].as_u64().unwrap() as u16,
                r[1].as_str().unwrap().to_owned(),
                headers,
            )
        })
        .collect();
    let mock = Mock {
        queue: Mutex::new(queue),
        requests: Mutex::default(),
        now: Arc::clone(&now),
        abort_at: case["abortAt"].as_u64().unwrap_or(0) as usize,
        signal: signal.clone(),
    };
    let env = FakeEnv {
        now,
        counter: Mutex::new(0),
    };
    let port = free_port();
    let script = Script {
        answers: Mutex::new(case["answers"].as_array().unwrap().clone()),
        recorded: Mutex::default(),
        hits: case["hits"]
            .as_array()
            .map(|h| h.iter().map(|p| p.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default(),
        port,
    };
    let mut ctx = LoginContext::new(&mock, &script, &env);
    ctx.signal = signal;
    ctx.callback_host = "127.0.0.1".into();
    ctx.callback_port = Some(port);
    let (result, error) = match provider.login(&ctx).await {
        Ok(credential) => (credential.to_json(), Value::Null),
        Err(e) => (Value::Null, json!(comparable(&e))),
    };
    let hits = script.recorded.lock().unwrap().hits.take();
    let hits = match hits {
        Some(handle) => handle.await.unwrap(),
        None => Vec::new(),
    };
    let recorded = script.recorded.lock().unwrap();
    let expected_error = case["error"].as_str().map_or(Value::Null, |e| json!(comparable(e)));
    assert_eq!(error, expected_error, "{name}: error");
    assert_eq!(result, case["result"], "{name}: result");
    assert_eq!(
        Value::Array(recorded.prompts.clone()),
        case["prompts"],
        "{name}: prompts"
    );
    assert_eq!(Value::Array(recorded.events.clone()), case["events"], "{name}: events");
    assert_eq!(
        Value::Array(mock.requests.lock().unwrap().clone()),
        case["requests"],
        "{name}: requests"
    );
    assert_eq!(
        Value::Array(hits),
        case["callbackResponses"],
        "{name}: callback responses"
    );
}

async fn replay_provider(provider: &str) {
    let fixture = fixture();
    let cases: Vec<&Value> = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["provider"] == provider)
        .collect();
    assert!(cases.len() >= 12, "{provider}: {} cases", cases.len());
    for case in cases {
        replay(case).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_oauth_login_anthropic_matches_typescript() {
    replay_provider("anthropic").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_oauth_login_codex_matches_typescript() {
    replay_provider("openai-codex").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_oauth_login_copilot_matches_typescript() {
    replay_provider("github-copilot").await;
}

#[test]
fn ai_oauth_login_pages_match_typescript() {
    let pages = &fixture()["pages"];
    assert_eq!(
        oauth_success_html("Done <now> & \"then\" 'go'"),
        pages["success"].as_str().unwrap()
    );
    assert_eq!(
        oauth_error_html("Failed.", Some("Error: <x>")),
        pages["error"].as_str().unwrap()
    );
    assert_eq!(
        oauth_error_html("Failed.", None),
        pages["errorNoDetails"].as_str().unwrap()
    );
}

#[test]
fn ai_oauth_login_pkce_is_rfc7636() {
    // RFC 7636 appendix B: these 32 bytes give this verifier and challenge.
    struct Bytes;
    impl LoginEnv for Bytes {
        fn now_ms(&self) -> f64 {
            0.0
        }
        fn sleep(&self, _: f64) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
        fn fill_random(&self, buf: &mut [u8]) {
            buf.copy_from_slice(&[
                116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212, 37, 77, 105, 214,
                191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
            ]);
        }
    }
    assert_eq!(
        generate_pkce_from(&Bytes),
        (
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_owned(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".to_owned()
        )
    );
}

#[test]
fn ai_oauth_login_parses_pasted_codes() {
    let parsed = |input: &str| {
        let p = parse_authorization_input(input);
        (p.code, p.state)
    };
    let some = |s: &str| Some(s.to_owned());
    assert_eq!(
        parsed("http://localhost:53692/callback?code=c&state=s"),
        (some("c"), some("s"))
    );
    // WHATWG reads `localhost:` as a scheme, so the query still parses.
    assert_eq!(
        parsed("localhost:53692/callback?code=c&state=s"),
        (some("c"), some("s"))
    );
    assert_eq!(parsed("?code=a+b%21&state=s"), (some("a b!"), some("s")));
    assert_eq!(parsed("c#s#more"), (some("c"), some("s")));
    assert_eq!(parsed(" bare "), (some("bare"), None));
    assert_eq!(parsed("   "), (None, None));
    assert_eq!(parsed("https://x.test/cb?state=s"), (None, some("s")));
}

#[test]
fn ai_oauth_login_callback_routes() {
    let anthropic = |target| callback_reply(CallbackKind::Anthropic, target, "st");
    assert_eq!(
        anthropic("/callback?code=c&state=st").code,
        Some(("c".into(), "st".into()))
    );
    assert_eq!(anthropic("/callback?code=c&state=st").status, 200);
    assert_eq!(anthropic("/callback?code=&state=st").status, 400);
    assert_eq!(anthropic("/callback?error=&code=c&state=st").status, 200);
    assert_eq!(anthropic("/elsewhere?code=c&state=st").status, 404);
    let codex = |target| callback_reply(CallbackKind::OpenAiCodex, target, "st");
    assert_eq!(
        codex("/auth/callback?code=c&state=st").code,
        Some(("c".into(), "st".into()))
    );
    assert_eq!(
        codex("/auth/callback?code=c").body,
        oauth_error_html("State mismatch.", None)
    );
    assert_eq!(
        codex("/auth/callback?state=st&code=").body,
        oauth_error_html("Missing authorization code.", None)
    );
}
