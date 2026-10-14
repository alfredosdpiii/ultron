//! Behaviors of the ported providers that the recorded fixtures leave out because they depend on timing or a live
//! socket: Claude Code warm spares (`startSpare`/`takeSpare`), and the Radius loopback OAuth callback server. Each
//! expectation is the TypeScript behavior, stated literally.

use ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, OAuthAuth};
use ai::claude_code_cli::{self, ClaudeCodeOptions};
use ai::http::{AbortSignal, BoxFuture, HttpRequest, HttpResponse, Transport};
use ai::{Model, TranscriptContext};
use serde_json::json;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

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
    // Hang guard only: under load the spare can take long to start.
    let wait_for = |prefix: &str, n: usize| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
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
    // Settle, then check no fourth process started (a short settle can only miss an extra spawn, never fail).
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(count("argv:"), 3);
    assert_eq!(count("stdin:"), 2);
    claude_code_cli::reset_claude_code_cli_state();
    assert_eq!(claude_code_cli::claude_code_cli_stats().1, 0);
}

/// The page the browser thread read back from the callback server, once it has read it (hang guard 120 s).
fn served_page(pages: &mpsc::Receiver<String>) -> String {
    pages
        .recv_timeout(Duration::from_secs(120))
        .expect("the browser read no page within 120 s")
}

/// A Radius login whose callback server listens on a free port the system picks, not the registered 1456.
///
/// 1456 is one port for the whole machine: other worktrees' test runs (older copies that do not take the shared port
/// lock) and a real login bind it too. One that bound it between this test's probe and its login's bind made the
/// login fall back to the pasted URL, and the browser's call reached no server ("port 1456 taken?"). A port of its
/// own leaves nothing to share; the redirect URI names the bound port (`with_callback_port(0)`).
fn radius(transport: Arc<GatewayTransport>) -> ai::oauth_radius::RadiusOAuth {
    ai::oauth_radius::RadiusOAuth::new("Radius", "gateway.example.com", transport, now).with_callback_port(0)
}

/// Runs a login with a 120 s hang guard.
fn login<T>(runtime: &tokio::runtime::Runtime, future: impl std::future::Future<Output = T>) -> T {
    runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(120), future).await })
        .expect("the login neither completed nor failed within 120 s")
}

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
/// When the call reaches no callback server of this login (the port was refused, or another login's server
/// answered), the login has fallen back to the manual prompt: the prompt then fails with that reason, so the test
/// fails at once instead of waiting out its hang guard.
struct Browser {
    pages: mpsc::Sender<String>,
    query: fn(&str) -> String,
    lost: AbortSignal,
    /// The callback server's port, from the "Listening for OAuth callback on ..." progress line.
    port: Mutex<Option<u16>>,
}

const LOST: &str = "the browser's callback reached no callback server of this login";

impl AuthInteraction for Browser {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            match prompt {
                AuthPrompt::Select { .. } => Ok("browser".into()),
                _ => {
                    // The manual prompt is withdrawn once the callback wins.
                    let cancel = cancel.expect("manual prompt is cancellable");
                    tokio::select! {
                        () = cancel.cancelled() => Err("cancelled".into()),
                        () = self.lost.cancelled() => Err(LOST.into()),
                    }
                }
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::Progress { message } = &event
            && let Some(url) = message.strip_prefix("Listening for OAuth callback on ")
        {
            *self.port.lock().unwrap() = reqwest::Url::parse(url).unwrap().port();
        }
        if let AuthEvent::AuthUrl { url, .. } = event {
            let state = reqwest::Url::parse(&url)
                .unwrap()
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            let query = (self.query)(&state);
            let pages = self.pages.clone();
            let lost = self.lost.clone();
            let port = self
                .port
                .lock()
                .unwrap()
                .expect("the callback server's port precedes the URL");
            std::thread::spawn(move || {
                let call = || -> std::io::Result<String> {
                    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
                    write!(
                        stream,
                        "GET /oauth/callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                    )?;
                    let mut page = String::new();
                    stream.read_to_string(&mut page)?;
                    Ok(page)
                };
                let page = call().unwrap_or_else(|error| format!("{LOST}: {error}"));
                // This login's server always answers, and checks the state it generated: no answer, or a state
                // mismatch, came from no server or another login's.
                if !page.starts_with("HTTP/1.1 ") || page.contains("OAuth state mismatch") {
                    lost.abort();
                }
                let _ = pages.send(page);
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
    let radius = radius(transport.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (pages, served) = mpsc::channel();
    let browser = Browser {
        pages,
        lost: AbortSignal::new(),
        port: Mutex::default(),
        query: |state| format!("code=browser-code&state={state}"),
    };
    let credential = login(&runtime, radius.login(&browser)).expect("login");
    assert_eq!(credential.access, "from-callback");
    let port = browser.port.lock().unwrap().expect("a listening line");
    assert_ne!(
        port, 1456,
        "the test's server must not share the machine-wide registered port"
    );
    assert_eq!(credential.expires, 1_767_225_600_000.0);
    let token = &transport.0.lock().unwrap()[1];
    assert!(token.body.contains("code=browser-code"), "{}", token.body);
    assert!(token.body.contains("grant_type=authorization_code"));
    // The redirect URI names the port the test's server listened on.
    assert!(
        token.body.contains(&format!(
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A{port}%2Foauth%2Fcallback"
        )),
        "{}",
        token.body
    );
    let page = served_page(&served);
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    assert!(page.contains("Signed in to Radius. You may now close this page."));
}

#[test]
fn ai_oauth_radius_browser_denial_ends_the_login() {
    let radius = radius(Arc::new(GatewayTransport(Mutex::default())));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (pages, served) = mpsc::channel();
    let browser = Browser {
        pages,
        lost: AbortSignal::new(),
        port: Mutex::default(),
        query: |state| format!("error=access_denied&error_description=User+said+no&state={state}"),
    };
    let error = login(&runtime, radius.login(&browser)).unwrap_err();
    assert_eq!(error, "OAuth callback did not complete.");
    let page = served_page(&served);
    assert!(page.starts_with("HTTP/1.1 400 Bad Request"), "{page}");
    assert!(page.contains("User said no"));
}
