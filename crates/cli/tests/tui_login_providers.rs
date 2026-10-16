//! The TUI login runner (`tui_login`) for every built-in account login. Each provider gets the OAuth handler
//! TypeScript registers (its `name` and `loginLabel` as fixtures/provider_names.json recorded them from the
//! TypeScript registry). Two full logins run through the runner and its dialog protocol against stubbed endpoints
//! (an in-memory transport, nothing leaves the process), and save what the TypeScript flow returned for the same
//! answers and responses: Anthropic's pasted redirect (crates/ai/fixtures/oauth-login.json, "anthropic manual
//! redirect url") and OpenRouter's pasted code (crates/ai/fixtures/oauth-openrouter.json, "paste bare code").

use ai::http::{BoxFuture, HttpRequest, HttpResponse, Transport};
use interactive::lane::{LaneEvent, LoginEvent, LoginPrompt, LoginStep};
use serde_json::Value;
use session_core::stored_auth::OAuthOptions;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

struct OneBody(Option<Vec<u8>>);

impl ai::http::Body for OneBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

/// Canned responses in order; records each request.
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
            Ok(HttpResponse {
                status,
                headers: Default::default(),
                body: Box::new(OneBody(Some(body.into_bytes()))),
            })
        })
    }
}

fn profile() -> (tempfile::TempDir, cli::Env) {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let dir = agent.display().to_string();
    let env = cli::Env {
        cwd: tmp.path().to_path_buf(),
        home: tmp.path().to_path_buf(),
        var: Arc::new(move |k| (k == "ULTRON_CODING_AGENT_DIR").then(|| dir.clone())),
        docs: tmp.path().join("docs"),
    };
    (tmp, env)
}

#[test]
fn cli_tui_login_offers_every_typescript_account_login() {
    let table: Value = serde_json::from_str(include_str!("../../interactive/fixtures/provider_names.json")).unwrap();
    let (_tmp, env) = profile();
    let mut checked = 0;
    for (id, provider) in table.as_object().unwrap() {
        let Some(oauth) = provider.get("oauth") else {
            assert!(
                !cli::tui_login::supported(&env, id),
                "{id} has no account login in TypeScript"
            );
            continue;
        };
        let handler = session_core::stored_auth::oauth_handler_with(&env, id, &OAuthOptions::default())
            .unwrap_or_else(|| panic!("{id}: no account login"));
        assert_eq!(handler.name(), oauth["name"].as_str().unwrap(), "{id}: name");
        assert_eq!(handler.login_label(), oauth["loginLabel"].as_str(), "{id}: login label");
        assert!(cli::tui_login::supported(&env, id));
        checked += 1;
    }
    assert_eq!(checked, 8);
}

/// Runs `provider`'s login through the runner, answering its prompts with `answer` (given the auth URL), and
/// returns the credential it saved.
fn login(provider: &str, responses: Vec<(u16, String)>, answer: impl Fn(&str, &LoginPrompt) -> String) -> Value {
    let (tmp, env) = profile();
    login_in(tmp, env, provider, responses, answer)
}

fn login_in(
    tmp: tempfile::TempDir,
    env: cli::Env,
    provider: &str,
    responses: Vec<(u16, String)>,
    answer: impl Fn(&str, &LoginPrompt) -> String,
) -> Value {
    let transport = Arc::new(Canned::default());
    *transport.queue.lock().unwrap() = responses;
    let (tx, rx) = mpsc::channel::<LaneEvent>();
    let tx = Mutex::new(tx);
    let send: Arc<dyn Fn(LaneEvent) + Send + Sync> = Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    });
    let slot = Arc::new(cli::tui_login::LoginSlot::default());
    let (saved_tx, saved_rx) = mpsc::channel::<()>();
    let saved_tx = Mutex::new(saved_tx);
    cli::tui_login::start_with(
        env,
        provider.into(),
        provider.into(),
        send,
        slot.clone(),
        Box::new(move || {
            let _ = saved_tx.lock().unwrap().send(());
        }),
        OAuthOptions {
            transport: transport.clone(),
            callback_port: Some(0),
        },
    );
    let guard = Duration::from_secs(120);
    let mut url = String::new();
    loop {
        match rx.recv_timeout(guard).expect("the login's next step within 120 s") {
            LaneEvent::Login(LoginStep::Event(LoginEvent::AuthUrl { url: shown, .. })) => url = shown,
            LaneEvent::Login(LoginStep::Prompt(prompt)) => slot.reply(Some(answer(&url, &prompt))),
            LaneEvent::Login(LoginStep::Finished) => break,
            LaneEvent::Status(status) => panic!("{provider}: {status}"),
            _ => {}
        }
    }
    saved_rx.recv_timeout(guard).expect("the credential was saved");
    let auth: Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("agent/auth.json")).unwrap()).unwrap();
    auth[provider].clone()
}

/// A query parameter of `url`, percent-decoded.
fn query(url: &str, key: &str) -> String {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    let raw = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.replace('+', " "))
        .unwrap();
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&raw[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// The recorded credential with its expiry left out (it is the recording's fake clock's).
fn without_expiry(mut credential: Value) -> Value {
    credential.as_object_mut().unwrap().remove("expires");
    credential
}

#[test]
fn cli_tui_login_anthropic_saves_what_typescript_returns() {
    let recorded: Value = serde_json::from_str(include_str!("../../ai/fixtures/oauth-login.json")).unwrap();
    let case = recorded["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "anthropic manual redirect url")
        .unwrap();
    let responses = case["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_u64().unwrap() as u16, r[1].as_str().unwrap().to_string()))
        .collect();
    let saved = login("anthropic", responses, |url, prompt| {
        assert!(matches!(prompt, LoginPrompt::ManualCode { .. }), "{prompt:?}");
        let template = case["answers"][0].as_str().unwrap();
        template
            .replace("{redirect}", &query(url, "redirect_uri"))
            .replace("{state}", &query(url, "state"))
    });
    assert!(saved["expires"].as_f64().unwrap() > 0.0);
    assert_eq!(without_expiry(saved), without_expiry(case["result"].clone()));
}

#[test]
fn cli_tui_login_openrouter_saves_what_typescript_returns() {
    let recorded: Value = serde_json::from_str(include_str!("../../ai/fixtures/oauth-openrouter.json")).unwrap();
    let case = recorded["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "paste bare code")
        .unwrap();
    let responses = case["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_u64().unwrap() as u16, r[1].as_str().unwrap().to_string()))
        .collect();
    let saved = login("openrouter", responses, |_, prompt| {
        assert!(matches!(prompt, LoginPrompt::ManualCode { .. }), "{prompt:?}");
        case["answer"].as_str().unwrap().to_string()
    });
    assert_eq!(
        ai::json::normalize_numbers(saved),
        ai::json::normalize_numbers(case["result"]["credential"].clone())
    );
}

/// The browser opener is found on the run's own PATH, never this process's: a test's environment (no PATH) opens
/// nothing on the developer's desktop, and a run with a PATH opens the authorization address there. Before this, the
/// login tests opened real OpenRouter and claude.ai sign-in pages.
#[test]
fn cli_tui_login_opens_the_browser_from_the_runs_own_path() {
    let recorded: Value = serde_json::from_str(include_str!("../../ai/fixtures/oauth-openrouter.json")).unwrap();
    let case = recorded["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "paste bare code")
        .unwrap();
    let responses = case["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_u64().unwrap() as u16, r[1].as_str().unwrap().to_string()))
        .collect();
    let tmp = tempfile::tempdir().unwrap();
    let (agent, bin, opened) = (
        tmp.path().join("agent"),
        tmp.path().join("bin"),
        tmp.path().join("opened"),
    );
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let opener = bin.join(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
    std::fs::write(&opener, format!("#!/bin/sh\necho \"$1\" >> '{}'\n", opened.display())).unwrap();
    std::fs::set_permissions(&opener, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (dir, path) = (agent.display().to_string(), bin.display().to_string());
    let env = cli::Env {
        cwd: tmp.path().to_path_buf(),
        home: tmp.path().to_path_buf(),
        var: Arc::new(move |k| match k {
            "ULTRON_CODING_AGENT_DIR" => Some(dir.clone()),
            "PATH" => Some(path.clone()),
            _ => None,
        }),
        docs: tmp.path().join("docs"),
    };
    let seen = std::sync::Mutex::new((String::new(), String::new()));
    login_in(tmp, env, "openrouter", responses, |url, _| {
        // The opener starts before the prompt; wait for its line (the profile is removed when the login returns).
        let guard = std::time::Instant::now();
        let text = loop {
            let text = std::fs::read_to_string(&opened).unwrap_or_default();
            if text.ends_with('\n') {
                break text;
            }
            assert!(guard.elapsed() < Duration::from_secs(120), "the opener never ran");
            std::thread::yield_now();
        };
        *seen.lock().unwrap() = (url.to_string(), text);
        case["answer"].as_str().unwrap().to_string()
    });
    let (url, text) = seen.into_inner().unwrap();
    assert!(url.starts_with("https://openrouter.ai/auth?callback_url="), "{url}");
    assert_eq!(text, format!("{url}\n"));
}

/// A browser login listens where the options say (`callback_port`; `ULTRON_OAUTH_CALLBACK_PORT` in the TUI): with a
/// free port chosen here, the browser's redirect to that port completes Anthropic's login without a paste.
#[test]
fn cli_tui_login_browser_callback_listens_on_the_chosen_port() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (tmp, env) = profile();
    let transport = Arc::new(Canned::default());
    *transport.queue.lock().unwrap() = vec![(
        200,
        r#"{"access_token":"cb-access","refresh_token":"cb-refresh","expires_in":3600}"#.to_string(),
    )];
    let (tx, rx) = mpsc::channel::<LaneEvent>();
    let tx = Mutex::new(tx);
    let send: Arc<dyn Fn(LaneEvent) + Send + Sync> = Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    });
    let slot = Arc::new(cli::tui_login::LoginSlot::default());
    cli::tui_login::start_with(
        env,
        "anthropic".into(),
        "Anthropic".into(),
        send,
        slot,
        Box::new(|| {}),
        OAuthOptions {
            transport,
            callback_port: Some(port),
        },
    );
    let mut url = String::new();
    loop {
        match rx
            .recv_timeout(Duration::from_secs(120))
            .expect("the login's next step within 120 s")
        {
            LaneEvent::Login(LoginStep::Event(LoginEvent::AuthUrl { url: shown, .. })) => url = shown,
            LaneEvent::Login(LoginStep::Prompt(LoginPrompt::ManualCode { .. })) => {
                let state = query(&url, "state");
                let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("the callback listens there");
                std::io::Write::write_all(
                    &mut stream,
                    format!("GET /callback?code=cb-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
                )
                .unwrap();
            }
            LaneEvent::Login(LoginStep::Finished) => break,
            LaneEvent::Status(status) => panic!("{status}"),
            _ => {}
        }
    }
    let auth: Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("agent/auth.json")).unwrap()).unwrap();
    assert_eq!(auth["anthropic"]["access"], "cb-access");
}
