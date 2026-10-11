//! Replays the `subscription` cases of `fixtures/oauth.json` (Anthropic, OpenAI Codex and GitHub Copilot refresh and
//! toAuth, recorded from the TypeScript OAuth flows by `fixtures/generate-rest.mjs oauth`).

use ai::http::{Body, BoxFuture, HttpRequest, HttpResponse, Transport};
use ai::oauth::{OAuthCredential, OAuthProvider, codex_account_id, copilot_base_url, normalize_domain};
use ai::oauth_flow::form_urlencode;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const NOW: f64 = 1_767_225_600_000.0;

struct OnceBody(Option<Vec<u8>>);

impl Body for OnceBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

#[derive(Default)]
struct Mock {
    queue: Mutex<Vec<(u16, String)>>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let next = {
            let mut queue = self.queue.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        Box::pin(async move {
            let (status, body) = next.ok_or("unexpected request")?;
            Ok(HttpResponse {
                status,
                headers: Default::default(),
                body: Box::new(OnceBody(Some(body.into_bytes()))),
            })
        })
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

fn subscription_cases() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/oauth.json")).unwrap();
    serde_json::from_value(fixture["subscription"].clone()).unwrap()
}

fn request_json(request: &HttpRequest) -> Value {
    json!({"url": request.url, "method": request.method, "headers": request.headers, "body": request.body})
}

#[tokio::test]
async fn ai_oauth_refresh_and_to_auth_match_typescript() {
    let cases = subscription_cases();
    assert_eq!(cases.len(), 22);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let provider = OAuthProvider::from_id(case["provider"].as_str().unwrap()).unwrap();
        let credential = OAuthCredential::from_json(&case["credential"]).unwrap();
        let mock = Mock::default();
        *mock.queue.lock().unwrap() = case["responses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r[0].as_u64().unwrap() as u16, r[1].as_str().unwrap().to_owned()))
            .collect();
        let (result, error) = if case["op"] == "refresh" {
            match provider.refresh(&mock, &credential, NOW, None).await {
                Ok(c) => (c.to_json(), Value::Null),
                Err(e) => (Value::Null, json!(comparable(&e))),
            }
        } else {
            let auth = provider.to_auth(&credential);
            let mut out = json!({"apiKey": auth.api_key});
            if let Some(base) = auth.base_url {
                out["baseUrl"] = json!(base);
            }
            (out, Value::Null)
        };
        let expected_error = case["error"].as_str().map_or(Value::Null, |e| json!(comparable(e)));
        assert_eq!(result, case["result"], "{name}: result");
        assert_eq!(error, expected_error, "{name}: error");
        let requests: Vec<Value> = mock.requests.lock().unwrap().iter().map(request_json).collect();
        assert_eq!(Value::Array(requests), case["requests"], "{name}: requests");
    }
}

#[test]
fn ai_oauth_form_encode_matches_url_search_params() {
    // new URLSearchParams({a: "r & =1", "k y": "ü~!'()*"}).toString()
    assert_eq!(
        form_urlencode(&[("a", "r & =1"), ("k y", "ü~!'()*")]),
        "a=r+%26+%3D1&k+y=%C3%BC%7E%21%27%28%29*"
    );
}

#[test]
fn ai_oauth_codex_account_id_reads_jwt_claim() {
    // Payload {"https://api.openai.com/auth":{"chatgpt_account_id":"acct_123"}}; node's atob accepts it bare and with
    // one `=`, and throws for `==` and for the URL-safe alphabet.
    let payload = "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xMjMifX0";
    assert_eq!(codex_account_id(&format!("h.{payload}.s")).as_deref(), Some("acct_123"));
    assert_eq!(
        codex_account_id(&format!("h.{payload}=.s")).as_deref(),
        Some("acct_123")
    );
    assert_eq!(codex_account_id(&format!("h.{payload}==.s")), None);
    assert_eq!(
        codex_account_id(&format!("h.{}.s", payload.replacen('Y', "-", 1))),
        None
    );
    assert_eq!(codex_account_id(&format!("h.{payload}.s.x")), None);
}

#[test]
fn ai_oauth_copilot_urls_and_domains() {
    assert_eq!(
        normalize_domain("  Company.GHE.com  ").as_deref(),
        Some("company.ghe.com")
    );
    assert_eq!(
        normalize_domain("https://ghe.example.com:8443/x").as_deref(),
        Some("ghe.example.com")
    );
    assert_eq!(normalize_domain("   "), None);
    assert_eq!(
        copilot_base_url(Some("a;proxy-ep=proxy.business.githubcopilot.com;b"), None),
        "https://api.business.githubcopilot.com"
    );
    assert_eq!(
        copilot_base_url(Some("proxy-ep=edge.example.com"), Some("ghe.io")),
        "https://edge.example.com"
    );
    assert_eq!(
        copilot_base_url(Some("proxy-ep=;x"), Some("ghe.io")),
        "https://copilot-api.ghe.io"
    );
    // /proxy-ep=([^;]+)/ skips an empty value and matches a later one.
    assert_eq!(copilot_base_url(Some("proxy-ep=;proxy-ep=x.y"), None), "https://x.y");
    assert_eq!(copilot_base_url(None, None), "https://api.individual.githubcopilot.com");
}

/// Sends to a local server instead of the provider host, through the real reqwest transport.
struct Rewrite {
    inner: ai::http::ReqwestTransport,
    from: &'static str,
    to: String,
}

impl Transport for Rewrite {
    fn send(&self, mut request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        request.url = request.url.replacen(self.from, &self.to, 1);
        self.inner.send(request)
    }
}

#[tokio::test]
async fn ai_oauth_refresh_over_http_against_a_local_server() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let cases = subscription_cases();
    let case = cases.iter().find(|c| c["name"] == "anthropic refresh ok").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let reply = case["responses"][0][1].as_str().unwrap().to_owned();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|v| v.parse().unwrap())
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                        reply.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                    return (head.to_owned(), body.to_owned());
                }
            }
        }
    });
    let transport = Rewrite {
        inner: ai::http::ReqwestTransport::new(None),
        from: "https://platform.claude.com",
        to: format!("http://127.0.0.1:{port}"),
    };
    let credential = OAuthCredential::from_json(&case["credential"]).unwrap();
    let refreshed = OAuthProvider::Anthropic
        .refresh(&transport, &credential, NOW, None)
        .await
        .unwrap();
    assert_eq!(refreshed.to_json(), case["result"]);
    let (head, body) = server.await.unwrap();
    let expected = &case["requests"][0];
    assert!(head.starts_with("POST /v1/oauth/token HTTP/1.1"), "{head}");
    assert_eq!(body, expected["body"].as_str().unwrap());
    for (name, value) in expected["headers"].as_object().unwrap() {
        let line = format!("{name}: {}", value.as_str().unwrap());
        assert!(head.to_ascii_lowercase().contains(&line.to_ascii_lowercase()), "{line}");
    }
}
