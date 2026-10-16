//! The interactive startup update check: replays fixtures/version-check.json (utils/version-check.ts
//! `checkForNewVersion`, recorded with the registry stubbed by fixtures/generate-version-check.mjs) against a
//! 127.0.0.1 fake registry that the run's environment names (`ULTRON_LATEST_VERSION_URL`); never the real one.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cli::Env;
use serde_json::{Value, json};

/// A registry answering each request with the next reply; it records each request's method, path and accept.
struct FakeRegistry {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn registry(replies: Vec<(u16, String)>) -> FakeRegistry {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut accept = Value::Null;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("accept")
                {
                    accept = json!(value.trim());
                }
            }
            let mut parts = line.split_whitespace();
            seen.lock().unwrap().push(json!({
                "method": parts.next(), "path": parts.next(), "accept": accept,
            }));
            let (status, body) = replies.next().unwrap_or((500, String::new()));
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    FakeRegistry { port, requests }
}

#[test]
fn cli_startup_version_check_matches_ts_check_for_new_version() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/version-check.json")).unwrap();
    let current = fixture["current"].as_str().unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let reply = &case["reply"];
        let body = match &reply["body"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let fake = registry(vec![(reply["status"].as_u64().unwrap() as u16, body)]);
        let mut vars: HashMap<String, String> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        vars.insert(
            cli::startup_checks::LATEST_VERSION_URL_VAR.into(),
            format!("http://127.0.0.1:{}/ultron-agent/latest", fake.port),
        );
        let env = Env {
            cwd: PathBuf::from("/nonexistent"),
            home: PathBuf::from("/nonexistent"),
            var: Arc::new(move |k| vars.get(k).cloned()),
            docs: PathBuf::new(),
        };
        let newer = cli::startup_checks::check_for_new_version(&env, current);
        assert_eq!(json!(newer), case["newer"], "{case}");
        let requests = fake.requests.lock().unwrap().clone();
        let want: Vec<Value> = case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| json!({"method": r["method"], "path": "/ultron-agent/latest", "accept": r["accept"]}))
            .collect();
        assert_eq!(requests, want, "{case}");
    }
}

/// Without the test hook the check asks the npm registry's `latest` document (`LATEST_VERSION_URL`).
#[test]
fn cli_startup_version_check_defaults_to_the_npm_registry() {
    let env = Env {
        cwd: PathBuf::from("/nonexistent"),
        home: PathBuf::from("/nonexistent"),
        var: Arc::new(|_| None),
        docs: PathBuf::new(),
    };
    assert_eq!(
        cli::startup_checks::latest_version_url(&env),
        "https://registry.npmjs.org/ultron-agent/latest"
    );
}
