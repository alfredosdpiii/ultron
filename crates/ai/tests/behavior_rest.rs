//! Behaviors of the ported providers that the recorded fixtures leave out because they depend on timing or a live
//! socket: Claude Code warm spares (`startSpare`/`takeSpare`), and the Radius loopback OAuth callback server. Each
//! expectation is the TypeScript behavior, stated literally.

use ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, OAuthAuth};
use ai::claude_code_cli::{self, ClaudeCodeOptions};
use ai::http::{AbortSignal, BoxFuture, HttpRequest, HttpResponse, Transport};
use ai::{Model, TranscriptContext};
use serde_json::json;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn cc_model() -> Model {
    serde_json::from_value(json!({
        "id": "claude-opus-5-5", "name": "Claude Opus 5.5", "api": "claude-code-cli", "provider": "claude-code",
        "baseUrl": "claude-code://cli", "reasoning": true, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 32000,
    }))
    .unwrap()
}

#[test]
fn ai_claude_code_cli_second_request_takes_the_warm_spare() {
    claude_code_cli::reset_claude_code_cli_state();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    std::fs::write(&log, "").unwrap();
    std::fs::write(
        dir.path().join("reply"),
        "{\"type\":\"result\",\"is_error\":false,\"result\":\"ok\",\"stop_reason\":\"end_turn\",\"usage\":{}}\n",
    )
    .unwrap();
    let bin = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/fake-claude/claude");
    let options: ClaudeCodeOptions = serde_json::from_value(json!({"env": {
        "ULTRON_CLAUDE_CODE_BIN": bin,
        "ULTRON_CLAUDE_CODE_WARM": "1",
        "CLAUDE_CONFIG_DIR": "/cfg/warm",
        "FAKE_CLAUDE_REPLY": dir.path().join("reply").to_string_lossy(),
        "FAKE_CLAUDE_LOG": log.to_string_lossy(),
    }}))
    .unwrap();
    let context: TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "q", "timestamp": 1}]})).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let count = |prefix: &str| {
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with(prefix))
            .count()
    };
    let wait_for = |prefix: &str, n: usize| {
        for _ in 0..500 {
            if count(prefix) >= n {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{prefix} never reached {n}: {}", std::fs::read_to_string(&log).unwrap());
    };
    let first = runtime.block_on(claude_code_cli::stream_simple(
        &cc_model(),
        &context,
        &options,
        &mut |_| {},
    ));
    assert_eq!(
        serde_json::to_value(&first.content).unwrap(),
        json!([{"type": "text", "text": "ok"}])
    );
    // After a request completes, one idle process with the same arguments starts ahead of time.
    wait_for("argv:", 2);
    assert_eq!(count("stdin:"), 1);
    assert_eq!(claude_code_cli::claude_code_cli_stats().1, 1);
    let second = runtime.block_on(claude_code_cli::stream_simple(
        &cc_model(),
        &context,
        &options,
        &mut |_| {},
    ));
    assert_eq!(
        serde_json::to_value(&second.content).unwrap(),
        json!([{"type": "text", "text": "ok"}])
    );
    // The second request wrote its prompt into the spare; a proven shape gets the next spare while it runs.
    wait_for("argv:", 3);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(count("argv:"), 3);
    assert_eq!(count("stdin:"), 2);
    claude_code_cli::reset_claude_code_cli_state();
    assert_eq!(claude_code_cli::claude_code_cli_stats().1, 0);
}

/// Both callback tests bind 127.0.0.1:1456.
static PORT: Mutex<()> = Mutex::new(());

struct OneBody(Option<Vec<u8>>);

impl ai::http::Body for OneBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

/// Answers the discovery and the token request; records requests.
struct GatewayTransport(Mutex<Vec<HttpRequest>>);

impl Transport for GatewayTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        let body = if request.url.ends_with("/v1/oauth") {
            json!({"authorizationEndpoint": "https://login.example.com/authorize"})
        } else {
            json!({"access_token": "from-callback", "refresh_token": "r", "expires_in": 60})
        };
        self.0.lock().unwrap().push(request);
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: Default::default(),
                body: Box::new(OneBody(Some(body.to_string().into_bytes()))),
            })
        })
    }
}

/// Chooses the browser login, never answers the manual prompt, and calls the callback URL like a browser would.
struct Browser {
    pages: Arc<Mutex<Vec<String>>>,
    query: fn(&str) -> String,
}

impl AuthInteraction for Browser {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            match prompt {
                AuthPrompt::Select { .. } => Ok("browser".into()),
                _ => {
                    // The manual prompt is withdrawn once the callback wins.
                    cancel.expect("manual prompt is cancellable").cancelled().await;
                    Err("cancelled".into())
                }
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::AuthUrl { url, .. } = event {
            let state = reqwest::Url::parse(&url)
                .unwrap()
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            let query = (self.query)(&state);
            let pages = self.pages.clone();
            std::thread::spawn(move || {
                let mut stream = std::net::TcpStream::connect(("127.0.0.1", 1456)).unwrap();
                write!(
                    stream,
                    "GET /oauth/callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                )
                .unwrap();
                let mut page = String::new();
                stream.read_to_string(&mut page).unwrap();
                pages.lock().unwrap().push(page);
            });
        }
    }

    fn signal(&self) -> Option<AbortSignal> {
        Some(AbortSignal::new())
    }
}

fn now() -> i64 {
    1_767_225_600_000
}

#[test]
fn ai_oauth_radius_browser_login_completes_through_the_loopback_callback() {
    let transport = Arc::new(GatewayTransport(Mutex::default()));
    let radius = ai::oauth_radius::RadiusOAuth::new("Radius", "gateway.example.com", transport.clone(), now);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pages = Arc::new(Mutex::new(Vec::new()));
    let browser = Browser {
        pages: pages.clone(),
        query: |state| format!("code=browser-code&state={state}"),
    };
    let _port = PORT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let credential = runtime.block_on(radius.login(&browser)).expect("login");
    assert_eq!(credential.access, "from-callback");
    assert_eq!(credential.expires, 1_767_225_600_000.0);
    let token = &transport.0.lock().unwrap()[1];
    assert!(token.body.contains("code=browser-code"), "{}", token.body);
    assert!(token.body.contains("grant_type=authorization_code"));
    std::thread::sleep(Duration::from_millis(50));
    let page = pages.lock().unwrap().join("");
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    assert!(page.contains("Signed in to Radius. You may now close this page."));
}

#[test]
fn ai_oauth_radius_browser_denial_ends_the_login() {
    let radius = ai::oauth_radius::RadiusOAuth::new(
        "Radius",
        "gateway.example.com",
        Arc::new(GatewayTransport(Mutex::default())),
        now,
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let pages = Arc::new(Mutex::new(Vec::new()));
    let browser = Browser {
        pages: pages.clone(),
        query: |state| format!("error=access_denied&error_description=User+said+no&state={state}"),
    };
    let _port = PORT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let error = runtime.block_on(radius.login(&browser)).unwrap_err();
    assert_eq!(error, "OAuth callback did not complete.");
    std::thread::sleep(Duration::from_millis(50));
    let page = pages.lock().unwrap().join("");
    assert!(page.starts_with("HTTP/1.1 400 Bad Request"), "{page}");
    assert!(page.contains("User said no"));
}
