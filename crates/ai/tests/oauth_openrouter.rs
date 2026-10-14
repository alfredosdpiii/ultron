//! Replays `fixtures/oauth-openrouter.json` (recorded from the TypeScript OpenRouter OAuth by
//! `fixtures/generate-oauth-openrouter.mjs`): the same paste answers and browser callback hits against the loopback
//! callback server on a system-chosen port, the same canned key-exchange responses. Prompts, notify events,
//! requests, the callback server's pages and the credential (or error) must match, with the callback port, the
//! random callback path and the PKCE values masked on both sides.

use ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, OAuthAuth};
use ai::http::{AbortSignal, BoxFuture, HttpRequest, HttpResponse, Transport};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct OneBody(Option<Vec<u8>>);

impl ai::http::Body for OneBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

/// Answers each request with the next canned response (`status: 0` is a network failure); records requests.
#[derive(Default)]
struct Canned {
    queue: Mutex<Vec<(u16, String)>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl Transport for Canned {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let next = {
            let mut queue = self.queue.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        Box::pin(async move {
            let (status, body) = next.ok_or("unexpected request")?;
            if status == 0 {
                return Err("fetch failed".into());
            }
            Ok(HttpResponse {
                status,
                headers: Default::default(),
                body: Box::new(OneBody(Some(body.into_bytes()))),
            })
        })
    }
}

/// The scripted user and browser of the recording.
struct Script {
    answer: Option<String>,
    hits: Vec<String>,
    prompts: Mutex<Vec<Value>>,
    events: Mutex<Vec<Value>>,
    callback: Mutex<Option<String>>,
    pages: Arc<Mutex<Vec<Value>>>,
    hits_done: AbortSignal,
}

fn get(url: &str) -> (u16, String) {
    let url = reqwest::Url::parse(url).unwrap();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", url.port().unwrap())).unwrap();
    let target = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

impl AuthInteraction for Script {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.prompts.lock().unwrap().push(prompt.to_json());
            let Some(answer) = self.answer.clone() else {
                cancel.expect("the paste prompt is withdrawable").cancelled().await;
                return Err("withdrawn".into());
            };
            if !self.hits.is_empty() {
                self.hits_done.cancelled().await;
            }
            let callback = self.callback.lock().unwrap().clone().unwrap_or_default();
            Ok(answer.replace("{callback}", &callback))
        })
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::AuthUrl { url, .. } = &event {
            let callback = reqwest::Url::parse(url)
                .unwrap()
                .query_pairs()
                .find(|(k, _)| k == "callback_url")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            *self.callback.lock().unwrap() = Some(callback.clone());
            let (hits, pages, done) = (self.hits.clone(), self.pages.clone(), self.hits_done.clone());
            std::thread::spawn(move || {
                for hit in hits {
                    let target = match hit.strip_prefix('@') {
                        Some(path) => reqwest::Url::parse(&callback).unwrap().join(path).unwrap().to_string(),
                        None => format!("{callback}?{hit}"),
                    };
                    let (status, body) = get(&target);
                    pages.lock().unwrap().push(json!({"status": status, "body": body}));
                }
                done.abort();
            });
        }
        self.events.lock().unwrap().push(event.to_json());
    }

    fn signal(&self) -> Option<AbortSignal> {
        Some(AbortSignal::new())
    }
}

/// Port, random callback path and PKCE values as placeholders.
fn mask(text: &str) -> String {
    let mut out = text.to_string();
    for (prefix, terminator) in [
        ("127.0.0.1:", '/'),
        ("127.0.0.1%3A", '%'),
        ("/oauth/callback/", '"'),
        ("%2Foauth%2Fcallback%2F", '&'),
        ("code_challenge=", '&'),
        ("\\\"code_verifier\\\":\\\"", '\\'),
    ] {
        let mut result = String::new();
        let mut rest = out.as_str();
        while let Some(at) = rest.find(prefix) {
            result.push_str(&rest[..at + prefix.len()]);
            let after = &rest[at + prefix.len()..];
            let len = after.find([terminator, '"', '?', ' ', '\\']).unwrap_or(after.len());
            result.push_str("<masked>");
            rest = &after[len..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

fn masked(value: &Value) -> Value {
    serde_json::from_str(&mask(&value.to_string())).unwrap()
}

#[test]
fn ai_oauth_openrouter_replays_recorded_typescript_flows() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/oauth-openrouter.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 13);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let transport = Arc::new(Canned::default());
        *transport.queue.lock().unwrap() = case["responses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r[0].as_u64().unwrap() as u16, r[1].as_str().unwrap().to_string()))
            .collect();
        let script = Script {
            answer: case["answer"].as_str().map(str::to_string),
            hits: case["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h.as_str().unwrap().to_string())
                .collect(),
            prompts: Mutex::default(),
            events: Mutex::default(),
            callback: Mutex::default(),
            pages: Arc::default(),
            hits_done: AbortSignal::new(),
        };
        let flow = ai::oauth_openrouter::OpenRouterOAuth {
            transport: transport.clone(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(120), flow.login(&script)).await })
            .expect("the login ended within 120 s");
        let (result, error) = match outcome {
            Ok(credential) => {
                let refreshed = runtime.block_on(flow.refresh(&credential, None)).unwrap();
                (
                    json!({
                        "credential": credential.to_json(),
                        "toAuth": serde_json::to_value(flow.to_auth(&credential).unwrap()).unwrap(),
                        "refreshed": refreshed.to_json(),
                    }),
                    Value::Null,
                )
            }
            Err(error) => (Value::Null, json!(error)),
        };
        if !script.hits.is_empty() {
            runtime
                .block_on(async { tokio::time::timeout(Duration::from_secs(120), script.hits_done.cancelled()).await })
                .expect("the browser's hits ended within 120 s");
        }
        let pages = script.pages.lock().unwrap().clone();
        let requests: Vec<Value> = transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                json!({"url": r.url, "method": r.method, "headers": r.headers,
                    "body": if r.body.is_empty() { Value::Null } else { json!(r.body) }})
            })
            .collect();
        assert_eq!(
            masked(&json!(*script.prompts.lock().unwrap())),
            masked(&case["prompts"]),
            "{name}: prompts"
        );
        assert_eq!(
            masked(&json!(*script.events.lock().unwrap())),
            masked(&case["events"]),
            "{name}: events"
        );
        assert_eq!(masked(&json!(requests)), masked(&case["requests"]), "{name}: requests");
        assert_eq!(json!(pages), case["callbackResponses"], "{name}: callback pages");
        assert_eq!(
            ai::json::normalize_numbers(result),
            ai::json::normalize_numbers(case["result"].clone()),
            "{name}: result"
        );
        assert_eq!(error, case["error"], "{name}: error");
    }
}

/// The key exchange answers only once `release` fires; `started` fires when it is asked.
struct Gated {
    started: AbortSignal,
    release: AbortSignal,
    requests: Mutex<Vec<HttpRequest>>,
}

impl Transport for Gated {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            self.started.abort();
            self.release.cancelled().await;
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: Box::new(OneBody(Some(br#"{"key":"sk-or-from-callback"}"#.to_vec()))),
            })
        })
    }
}

/// Answers the paste prompt only once the browser's callback is already exchanging its code.
struct LatePaste {
    started: AbortSignal,
    release: AbortSignal,
    page: Arc<Mutex<Option<u16>>>,
}

impl AuthInteraction for LatePaste {
    fn prompt<'a>(
        &'a self,
        _prompt: AuthPrompt,
        _cancel: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.started.cancelled().await;
            // The exchange resumes after this answer has been taken (the login handles it first).
            self.release.abort();
            Ok("pasted-code".into())
        })
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::AuthUrl { url, .. } = &event {
            let callback = reqwest::Url::parse(url)
                .unwrap()
                .query_pairs()
                .find(|(k, _)| k == "callback_url")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            let page = self.page.clone();
            std::thread::spawn(move || {
                let (status, _) = get(&format!("{callback}?code=browser-code"));
                *page.lock().unwrap() = Some(status);
            });
        }
    }

    fn signal(&self) -> Option<AbortSignal> {
        Some(AbortSignal::new())
    }
}

/// `cancelWait` (openrouter.ts): "A claimed callback is already exchanging its code; let that exchange settle the
/// login." A paste answered while the callback's exchange runs neither replaces it nor starts a second exchange.
#[test]
fn ai_oauth_openrouter_paste_after_a_claimed_callback_keeps_the_callbacks_exchange() {
    let (started, release) = (AbortSignal::new(), AbortSignal::new());
    let transport = Arc::new(Gated {
        started: started.clone(),
        release: release.clone(),
        requests: Mutex::default(),
    });
    let flow = ai::oauth_openrouter::OpenRouterOAuth {
        transport: transport.clone(),
    };
    let script = LatePaste {
        started,
        release,
        page: Arc::default(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let credential = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(120), flow.login(&script)).await })
        .expect("the login ended within 120 s")
        .expect("the callback's exchange settles the login");
    assert_eq!(credential.access, "sk-or-from-callback");
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one exchange");
    assert!(requests[0].body.contains("browser-code"), "{}", requests[0].body);
}
