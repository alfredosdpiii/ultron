//! Ported from packages/coding-agent/test/mcp-oauth-refresh.test.ts and mcp-oauth-store.test.ts with their literal
//! expectations, against fixtures/fake_oauth_server.py (the port of test/suite/mcp-oauth-server.ts). Stores sharing
//! the credential file and lock directory stand in for separate processes, as there.

use mcp::CancelToken;
use mcp::http::{AuthProvider, Unauthorized};
use mcp::oauth::flow::StateStore;
use mcp::oauth::signin::{OAuthAuth, OAuthSettings, SignInPrompt, sign_in};
use mcp::oauth::store::CredentialStore;
use mcp::oauth::{Challenge, OAuthError};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

struct Server {
    child: Child,
    origin: String,
}

impl Server {
    fn start(args: &[&str]) -> Server {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake_oauth_server.py");
        let mut child = Command::new(python())
            .arg(script)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Server {
            child,
            origin: format!("http://127.0.0.1:{}", line.trim()),
        }
    }

    fn url(&self) -> String {
        format!("{}/mcp", self.origin)
    }

    fn log(&self) -> Vec<String> {
        let text = reqwest::blocking::get(format!("{}/__log", self.origin))
            .unwrap()
            .text()
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn expire_access_tokens(&self) {
        reqwest::blocking::Client::new()
            .post(format!("{}/__expire", self.origin))
            .body("{}")
            .send()
            .unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The browser follows the authorization URL; nobody pastes a redirect URL.
#[derive(Default)]
struct Browser {
    opened: Mutex<Vec<String>>,
}

impl SignInPrompt for Browser {
    fn show_authorization_url(&self, url: &str) {
        self.opened.lock().unwrap().push(url.to_string());
        let url = url.to_string();
        std::thread::spawn(move || {
            let _ = reqwest::blocking::get(url);
        });
    }

    fn prompt_for_redirect_url(&self, cancel: &CancelToken) -> Option<String> {
        let start = Instant::now();
        while !cancel.is_cancelled() {
            assert!(start.elapsed() < Duration::from_secs(120), "the sign-in never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }
}

fn store(dir: &Path) -> Arc<CredentialStore> {
    Arc::new(CredentialStore::new(dir, None))
}

fn signed_in(server: &Server, dir: &Path) {
    let browser = Browser::default();
    sign_in(
        &server.url(),
        &store(dir).for_server("test", &server.url()),
        &OAuthSettings::default(),
        None,
        &browser,
    )
    .unwrap();
}

fn provider(server: &Server, dir: &Path) -> OAuthAuth {
    OAuthAuth::new(
        &server.url(),
        store(dir).for_server("test", &server.url()),
        Box::new(|| Ok(OAuthSettings::default())),
        Arc::default(),
    )
}

fn rejected(server: &Server, token: &str) -> Unauthorized {
    Unauthorized {
        status: 401,
        challenge: None,
        server_url: server.url(),
        token: Some(token.into()),
    }
}

fn lock_entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "lock"))
        .collect()
}

#[test]
fn mcp_oauth_refreshes_once_when_several_processes_find_the_same_token_rejected() {
    let server = Server::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    signed_in(&server, dir.path());
    server.expire_access_tokens();
    let processes = [
        provider(&server, dir.path()),
        provider(&server, dir.path()),
        provider(&server, dir.path()),
    ];
    // The server rotates refresh tokens: a second refresh with refresh-1 would fail with invalid_grant.
    std::thread::scope(|scope| {
        for process in &processes {
            let context = rejected(&server, "access-1");
            scope.spawn(move || process.on_unauthorized(&context).unwrap());
        }
    });
    let refreshes = server.log().iter().filter(|e| e.starts_with("token refresh ")).count();
    assert_eq!(refreshes, 1);
    for process in &processes {
        assert_eq!(process.token().unwrap().as_deref(), Some("access-2"));
    }
    // The locks are released.
    assert_eq!(lock_entries(dir.path()), Vec::<PathBuf>::new());
}

#[test]
fn mcp_oauth_waits_for_a_running_refresh_to_save_the_new_tokens() {
    let server = Server::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    signed_in(&server, dir.path());
    server.expire_access_tokens();
    let auth = provider(&server, dir.path());
    auth.on_unauthorized(&rejected(&server, "access-1")).unwrap();
    let state = store(dir.path())
        .for_server("test", &server.url())
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(state["tokens"]["access_token"], "access-2");
}

#[test]
fn mcp_oauth_rejects_an_authorization_response_from_another_issuer() {
    let server = Server::start(&["--iss", "https://attacker.example"]);
    let dir = tempfile::tempdir().unwrap();
    let error = sign_in(
        &server.url(),
        &store(dir.path()).for_server("test", &server.url()),
        &OAuthSettings::default(),
        None,
        &Browser::default(),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "OAuth issuer mismatch: expected \"{}\", received \"https://attacker.example\"",
            server.origin
        )
    );
}

#[test]
fn mcp_oauth_keeps_the_granted_scope_when_the_server_asks_for_more() {
    let server = Server::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path()).for_server("test", &server.url());
    let browser = Browser::default();
    let sign_in_with = |challenge: Challenge| {
        sign_in(
            &server.url(),
            &store,
            &OAuthSettings::default(),
            Some(&challenge),
            &browser,
        )
        .unwrap();
    };
    sign_in_with(Challenge {
        scope: Some("issues:read".into()),
        ..Challenge::default()
    });
    // The token response names no scope, so the grant has the requested one.
    assert_eq!(store.load().unwrap().unwrap()["tokens"]["scope"], "issues:read");
    // The step-up challenge lists only the missing scope; requesting just that would lose issues:read.
    sign_in_with(Challenge {
        error: Some("insufficient_scope".into()),
        scope: Some("issues:write".into()),
        ..Challenge::default()
    });
    let scopes: Vec<Option<String>> = browser
        .opened
        .lock()
        .unwrap()
        .iter()
        .map(|u| {
            url::Url::parse(u)
                .unwrap()
                .query_pairs()
                .find(|(k, _)| k == "scope")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(
        scopes,
        [
            Some("issues:read".to_string()),
            Some("issues:read issues:write".to_string())
        ]
    );
    assert_eq!(
        store.load().unwrap().unwrap()["tokens"]["scope"],
        "issues:read issues:write"
    );
}

#[test]
fn mcp_oauth_uses_the_configured_authorization_server_metadata_url() {
    let server = Server::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    let settings = OAuthSettings {
        auth_server_metadata_url: Some(format!("{}/missing", server.origin)),
        ..OAuthSettings::default()
    };
    let error = sign_in(
        &server.url(),
        &store(dir.path()).for_server("test", &server.url()),
        &settings,
        None,
        &Browser::default(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("HTTP 404 loading authorization server metadata"),
        "{error}"
    );
}

#[test]
fn mcp_oauth_an_insufficient_scope_challenge_needs_sign_in() {
    let server = Server::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    let auth = provider(&server, dir.path());
    let context = Unauthorized {
        status: 403,
        challenge: Some("Bearer error=\"insufficient_scope\", scope=\"issues:write\"".into()),
        server_url: server.url(),
        token: Some("access-1".into()),
    };
    let error = auth.on_unauthorized(&context).unwrap_err();
    assert_eq!(error.to_string(), OAuthError::AuthorizationRequired.to_string());
}

const SERVER_URL: &str = "https://mcp.example.com/mcp";

fn state(access_token: &str) -> Value {
    json!({ "serverUrl": SERVER_URL, "tokens": { "access_token": access_token, "token_type": "Bearer" } })
}

fn stored_keys(dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(dir.join("mcp-auth.json")).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    value.as_object().unwrap().keys().cloned().collect()
}

#[test]
fn mcp_oauth_store_keeps_separate_credentials_for_servers_sharing_a_url() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.for_server("work", SERVER_URL).save(&state("work-token")).unwrap();
    store
        .for_server("personal", SERVER_URL)
        .save(&state("personal-token"))
        .unwrap();
    let token = |name: &str| {
        store
            .for_server(name, SERVER_URL)
            .load()
            .unwrap()
            .map(|s| s["tokens"]["access_token"].clone())
    };
    assert_eq!(token("work"), Some(json!("work-token")));
    assert_eq!(token("personal"), Some(json!("personal-token")));
    assert!(store.remove("work", SERVER_URL).unwrap());
    assert_eq!(store.for_server("work", SERVER_URL).load().unwrap(), None);
    assert_eq!(
        store.tokens("personal", SERVER_URL).unwrap().unwrap()["access_token"],
        "personal-token"
    );
}

#[test]
fn mcp_oauth_store_moves_credentials_stored_by_url_to_the_first_server_that_loads_them() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mcp-auth.json"),
        json!({ SERVER_URL: state("legacy-token") }).to_string(),
    )
    .unwrap();
    let store = store(dir.path());
    // Reading tokens does not take the legacy state over.
    assert_eq!(
        store.tokens("work", SERVER_URL).unwrap().unwrap()["access_token"],
        "legacy-token"
    );
    assert_eq!(stored_keys(dir.path()), [SERVER_URL]);
    let loaded = store.for_server("my_work", SERVER_URL).load().unwrap().unwrap();
    assert_eq!(loaded["tokens"]["access_token"], "legacy-token");
    // Names differing only in `-` and `_` are the same server.
    assert_eq!(
        store.tokens("my-work", SERVER_URL).unwrap().unwrap()["access_token"],
        "legacy-token"
    );
    assert_eq!(store.for_server("personal", SERVER_URL).load().unwrap(), None);
    assert_eq!(stored_keys(dir.path()), [format!("mcp__my_work|{SERVER_URL}")]);
}

#[test]
fn mcp_oauth_store_signs_out_of_credentials_stored_by_url() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mcp-auth.json"),
        json!({ SERVER_URL: state("legacy-token") }).to_string(),
    )
    .unwrap();
    let store = store(dir.path());
    assert!(store.remove("work", SERVER_URL).unwrap());
    assert_eq!(stored_keys(dir.path()), Vec::<String>::new());
    assert!(!store.remove("work", SERVER_URL).unwrap());
}

/// The refresh lock is renewed while held (proper-lockfile renews it every `update`, half of `stale`): another process
/// waiting for it never takes it over mid-refresh, however long the refresh runs.
#[test]
fn mcp_oauth_a_held_refresh_lock_is_renewed_and_not_taken_over() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let timed = || {
        Arc::new(
            CredentialStore::new(dir.path(), None)
                .with_refresh_timing(Duration::from_secs(2), Duration::from_millis(100)),
        )
    };
    let first = timed().for_server("test", SERVER_URL);
    let second = timed().for_server("test", SERVER_URL);
    let lock = first.refresh_lock_path().unwrap();
    let (held, released) = (AtomicBool::new(false), AtomicBool::new(false));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            first
                .with_refresh_lock(|| {
                    held.store(true, Ordering::SeqCst);
                    // Hold well past the staleness period, counted in renewals of the lock's mtime.
                    let mut seen = std::fs::metadata(&lock).unwrap().modified().unwrap();
                    let mut renewals = 0;
                    let start = Instant::now();
                    while renewals < 30 {
                        assert!(start.elapsed() < Duration::from_secs(120), "the lock was not renewed");
                        let modified = std::fs::metadata(&lock).unwrap().modified().unwrap();
                        if modified != seen {
                            renewals += 1;
                            seen = modified;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    released.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
        });
        let start = Instant::now();
        while !held.load(Ordering::SeqCst) {
            assert!(start.elapsed() < Duration::from_secs(120));
            std::thread::sleep(Duration::from_millis(5));
        }
        second
            .with_refresh_lock(|| {
                assert!(released.load(Ordering::SeqCst), "the lock was taken over mid-refresh");
                Ok(())
            })
            .unwrap();
    });
    assert!(!lock.exists());
}

/// Refreshes are serialized per server, as each TypeScript auth provider shares its own refresh: one server's
/// refresh does not wait for another's.
#[test]
fn mcp_oauth_refreshes_of_different_servers_do_not_wait_for_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let (one, two) = (store.for_server("one", SERVER_URL), store.for_server("two", SERVER_URL));
    one.with_refresh_lock(|| {
        let (done, finished) = std::sync::mpsc::channel();
        let two = two.clone();
        std::thread::spawn(move || {
            let _ = done.send(two.with_refresh_lock(|| Ok(())));
        });
        finished
            .recv_timeout(Duration::from_secs(120))
            .expect("the other server's refresh waited")
            .unwrap();
        Ok(())
    })
    .unwrap();
}
