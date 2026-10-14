//! The Radius browser login on a callback port the system chooses (`with_callback_port(0)`): the authorize URL, the
//! listening notice, the paste prompt's placeholder and the token request all name the port the callback server
//! bound, and a browser redirect to it completes the login. No test holds the registered 1456, so these run in
//! parallel with anything else on the machine. The flow's request shapes against the fixed port are replayed from
//! the TypeScript recording in providers_rest.rs.

use ai::auth::{AuthEvent, AuthInteraction, AuthPrompt, OAuthAuth};
use ai::http::{AbortSignal, BoxFuture, HttpRequest, HttpResponse, Transport};
use serde_json::json;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

struct OneBody(Option<Vec<u8>>);

impl ai::http::Body for OneBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

/// Answers the discovery and the token request; records requests.
#[derive(Default)]
struct Gateway(Mutex<Vec<HttpRequest>>);

impl Transport for Gateway {
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

/// Picks the browser login, records what the flow says, and follows the authorize URL's redirect like a browser.
struct Browser {
    events: Mutex<Vec<AuthEvent>>,
    placeholder: Mutex<Option<String>>,
    pages: mpsc::Sender<String>,
}

impl AuthInteraction for Browser {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            match prompt {
                AuthPrompt::Select { .. } => Ok("browser".into()),
                AuthPrompt::ManualCode { placeholder, .. } => {
                    *self.placeholder.lock().unwrap() = placeholder;
                    cancel
                        .expect("the paste prompt is withdrawn once the callback wins")
                        .cancelled()
                        .await;
                    Err("cancelled".into())
                }
                other => Err(format!("unexpected prompt {other:?}")),
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::AuthUrl { url, .. } = &event {
            let url = reqwest::Url::parse(url).unwrap();
            let param = |key: &str| url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
            let (redirect, state) = (param("redirect_uri").unwrap(), param("state").unwrap());
            let redirect = reqwest::Url::parse(&redirect).unwrap();
            let port = redirect.port().unwrap();
            let pages = self.pages.clone();
            std::thread::spawn(move || {
                let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(
                    stream,
                    "GET /oauth/callback?code=browser-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                )
                .unwrap();
                let mut page = String::new();
                stream.read_to_string(&mut page).unwrap();
                let _ = pages.send(page);
            });
        }
        self.events.lock().unwrap().push(event);
    }

    fn signal(&self) -> Option<AbortSignal> {
        Some(AbortSignal::new())
    }
}

fn now() -> i64 {
    1_767_225_600_000
}

#[test]
fn ai_oauth_radius_browser_login_on_a_system_chosen_callback_port() {
    let gateway = Arc::new(Gateway::default());
    let radius = ai::oauth_radius::RadiusOAuth::new("My Radius", "http://127.0.0.1:9", gateway.clone(), now)
        .with_callback_port(0);
    let (pages, served) = mpsc::channel();
    let browser = Browser {
        events: Mutex::default(),
        placeholder: Mutex::default(),
        pages,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let credential = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(120), radius.login(&browser)).await })
        .expect("the login ended within 120 s")
        .expect("login");
    assert_eq!(credential.access, "from-callback");
    let page = served.recv_timeout(Duration::from_secs(120)).unwrap();
    assert!(page.starts_with("HTTP/1.1 200 OK"), "{page}");
    let events = browser.events.lock().unwrap();
    let AuthEvent::AuthUrl { url, .. } = events.iter().find(|e| matches!(e, AuthEvent::AuthUrl { .. })).unwrap() else {
        unreachable!()
    };
    let redirect = reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "redirect_uri")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    let port = reqwest::Url::parse(&redirect).unwrap().port().unwrap();
    assert_ne!(port, 1456);
    assert_eq!(redirect, format!("http://127.0.0.1:{port}/oauth/callback"));
    assert!(events.iter().any(
        |e| matches!(e, AuthEvent::Progress { message } if *message == format!("Listening for OAuth callback on {redirect}"))
    ));
    assert_eq!(browser.placeholder.lock().unwrap().as_deref(), Some(redirect.as_str()));
    let requests = gateway.0.lock().unwrap();
    let token = requests.iter().find(|r| r.url.ends_with("/v1/oauth/token")).unwrap();
    let encoded: String = reqwest::Url::parse_with_params("http://x/", [("r", &redirect)])
        .unwrap()
        .query()
        .unwrap()
        .trim_start_matches("r=")
        .to_string();
    assert!(
        token.body.contains(&format!("redirect_uri={encoded}")),
        "{}",
        token.body
    );
    assert!(token.body.contains("code=browser-code"), "{}", token.body);
}
