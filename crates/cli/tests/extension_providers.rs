//! Providers an extension declares (`capabilities.providers`, TypeScript `pi.registerProvider`) in a session and in
//! `--list-models`: the session can start on one, list it among its available models, switch to it, and its requests
//! go to the declared base URL with the declared api, key and headers. A registration that does not compose is a
//! load diagnostic (`Extension "<path>" error: ...`). The composition itself is compared with the TypeScript model
//! runtime in crates/ai/tests/extension_providers.rs.

mod common;

use serde_json::{Value, json};
use session_core::core::{Input, Op};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HANG: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn probe() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py")
}

/// One request the fake provider saw: the request line's path, the lowercased headers and the JSON body.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// An Anthropic-style server on loopback answering every request with `text`.
fn provider_server() -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let (mut headers, mut length) = (Vec::new(), 0);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':') {
                    let (k, v) = (k.trim().to_lowercase(), v.trim().to_string());
                    if k == "content-length" {
                        length = v.parse().unwrap();
                    }
                    headers.push((k, v));
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            shared.lock().unwrap().push(Seen { path, headers, body });
            let reply = common::anthropic_text("from the extension provider");
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(reply.as_bytes()).unwrap();
        }
    });
    (port, seen)
}

/// The probe extension declaring `providers` (written under the profile's extensions directory).
fn declare(agent_dir: &Path, providers: Value) {
    let dir = agent_dir.join("extensions").join("probe");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = json!({
        "name": "probe",
        "command": python(),
        "args": ["-I", probe().to_string_lossy()],
        "env": {"PROBE_CAPABILITIES": json!({"providers": providers}).to_string()},
        "startupTimeoutMs": 120_000,
        "eventTimeoutMs": 120_000,
    });
    std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
}

fn proxy(port: u16) -> Value {
    json!([{
        "name": "ext-proxy",
        "baseUrl": format!("http://127.0.0.1:{port}/proxy"),
        "api": "anthropic-messages",
        "apiKey": "ext-key",
        "headers": {"x-ext": "$EXT_HEADER"},
        "models": [{
            "id": "proxied", "name": "Proxied Model", "reasoning": false, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 50000, "maxTokens": 2000,
        }],
    }])
}

fn env(sandbox: &common::Sandbox) -> cli::Env {
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), sandbox.root.display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            sandbox.agent_dir.display().to_string(),
        ),
        (
            "ULTRON_SERVER_DIR".into(),
            sandbox.root.join("server").display().to_string(),
        ),
        ("ANTHROPIC_API_KEY".into(), "test-key".into()),
        ("EXT_HEADER".into(), "header-value".into()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PI_OFFLINE".into(), "1".into()),
    ];
    cli::Env {
        cwd: sandbox.cwd.clone(),
        home: sandbox.root.clone(),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    }
}

struct Actor {
    tx: Sender<Input>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Actor {
    fn open(sandbox: &common::Sandbox, model: &str) -> Actor {
        let repo = session_journal::SessionRepo::new(&sandbox.sessions_dir(), session_journal::SystemClock::shared());
        let metadata = repo
            .create(&sandbox.cwd.display().to_string(), None, None)
            .unwrap()
            .metadata()
            .clone();
        let open = session_core::core::OpenOptions {
            env: env(sandbox),
            flags: session_core::ModelFlags {
                model: Some(model.into()),
                ..Default::default()
            },
            session_root: sandbox.sessions_dir(),
            metadata,
            mode: "rpc".into(),
            services: cli::compose::Services::default(),
            auto_name: false,
        };
        let (tx, handle, _) = session_core::core::spawn(open, Box::new(|_| {})).unwrap();
        Actor {
            tx,
            handle: Some(handle),
        }
    }

    fn call(&self, op: Op) -> Result<Option<Value>, String> {
        session_core::core::call(&self.tx, op)
    }

    fn prompt(&self, text: &str) {
        let ran = self
            .call(Op::PromptAndWait {
                message: text.into(),
                images: Vec::new(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(ran["error"], Value::Null, "{ran}");
    }

    fn close(mut self) {
        let (done, closed) = std::sync::mpsc::channel();
        self.tx.send(Input::Close(done)).unwrap();
        closed.recv_timeout(HANG).unwrap();
        self.handle.take().unwrap().join().unwrap();
    }
}

fn refs(models: &Value) -> Vec<String> {
    models
        .as_array()
        .unwrap()
        .iter()
        .map(|m| format!("{}/{}", m["provider"].as_str().unwrap(), m["id"].as_str().unwrap()))
        .collect()
}

#[test]
fn cli_extension_provider_model_starts_a_session_and_receives_its_requests() {
    let (port, seen) = provider_server();
    let sandbox = common::Sandbox::new(port);
    declare(&sandbox.agent_dir, proxy(port));
    let actor = Actor::open(&sandbox, "ext-proxy/proxied");
    let models = actor.call(Op::AvailableModels).unwrap().unwrap();
    assert!(refs(&models).contains(&"ext-proxy/proxied".to_string()), "{models}");
    let proxied = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["provider"] == "ext-proxy")
        .unwrap();
    assert_eq!(
        (&proxied["name"], &proxied["baseUrl"], &proxied["contextWindow"]),
        (
            &json!("Proxied Model"),
            &json!(format!("http://127.0.0.1:{port}/proxy")),
            &json!(50000)
        )
    );
    actor.prompt("hello");
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let request = &requests[0];
    assert_eq!(request.path, "/proxy/v1/messages?beta=true");
    assert_eq!(request.header("x-api-key"), Some("ext-key"));
    assert_eq!(request.header("x-ext"), Some("header-value"));
    assert_eq!(request.body["model"], "proxied");
    actor.close();
}

#[test]
fn cli_extension_provider_model_is_selectable_from_a_builtin_model() {
    let (port, seen) = provider_server();
    let sandbox = common::Sandbox::new(port);
    declare(&sandbox.agent_dir, proxy(port));
    let actor = Actor::open(&sandbox, "anthropic/claude-sonnet-4-5");
    actor.prompt("first");
    let selected = actor
        .call(Op::SetModel {
            provider: "ext-proxy".into(),
            model_id: "proxied".into(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        (&selected["provider"], &selected["id"]),
        (&json!("ext-proxy"), &json!("proxied"))
    );
    actor.prompt("second");
    let paths: Vec<(String, Value, Option<String>)> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s.path.clone(),
                s.body["model"].clone(),
                s.header("x-api-key").map(str::to_string),
            )
        })
        .collect();
    assert_eq!(
        paths,
        [
            (
                "/v1/messages?beta=true".to_string(),
                json!("claude-sonnet-4-5"),
                Some("test-key".to_string())
            ),
            (
                "/proxy/v1/messages?beta=true".to_string(),
                json!("proxied"),
                Some("ext-key".to_string())
            ),
        ]
    );
    actor.close();
}

#[test]
fn cli_extension_provider_that_does_not_compose_is_a_load_diagnostic() {
    let sandbox = common::Sandbox::new(9);
    declare(
        &sandbox.agent_dir,
        json!([{"name": "broken", "baseUrl": "http://127.0.0.1:9", "apiKey": "k", "models": [{"id": "x"}]}]),
    );
    let runtime = session_core::compose::Runtime::new(
        &env(&sandbox),
        "print",
        sandbox.root.join("sessions"),
        cli::compose::Services::default(),
    )
    .unwrap();
    let manifest = sandbox.agent_dir.join("extensions/probe/extension.json");
    assert_eq!(
        runtime.diagnostics,
        [format!(
            "Extension \"{}\" error: Provider broken, model x: no \"api\" specified. Set at provider or model level.",
            manifest.display()
        )]
    );
    // It is left out of the registry; nothing else changes.
    let available = session_core::lane::available_models(&env(&sandbox));
    assert!(!available.iter().any(|m| m.provider == "broken"));
    assert!(available.iter().any(|m| m.provider == "anthropic"));
    drop(runtime);
}

#[test]
fn cli_extension_providers_leave_the_registry_when_the_session_ends() {
    let sandbox = common::Sandbox::new(9);
    declare(&sandbox.agent_dir, proxy(9));
    let listed = |env: &cli::Env| {
        session_core::lane::available_models(env)
            .iter()
            .any(|m| m.provider == "ext-proxy" && m.id == "proxied")
    };
    let runtime = session_core::compose::Runtime::new(
        &env(&sandbox),
        "print",
        sandbox.root.join("sessions"),
        cli::compose::Services::default(),
    )
    .unwrap();
    assert!(listed(&env(&sandbox)));
    // Another profile does not see them.
    let other = common::Sandbox::new(9);
    assert!(!listed(&env(&other)));
    drop(runtime);
    assert!(!listed(&env(&sandbox)));
}

fn list_models(sandbox: &common::Sandbox, extra: &[&str]) -> String {
    let out = sandbox
        .ultron()
        .args(["--list-models", "proxied"])
        .args(extra)
        .env("PI_OFFLINE", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn cli_list_models_shows_extension_providers() {
    let sandbox = common::Sandbox::new(9);
    declare(&sandbox.agent_dir, proxy(9));
    assert_eq!(
        list_models(&sandbox, &[]),
        "provider   model    context  max-out  thinking  images\next-proxy  proxied  50K      2K       no        no    \n"
    );
    assert_eq!(
        list_models(&sandbox, &["--no-extensions"]),
        "No models matching \"proxied\"\n"
    );
}

/// The session's model list is read when asked: a credential saved after the session opened makes its provider's
/// models available, and selectable even before the list is asked for again (as extension providers that change
/// while it runs).
#[test]
fn cli_session_available_models_follow_credentials_saved_later() {
    let (port, seen) = provider_server();
    let sandbox = common::Sandbox::new(port);
    let actor = Actor::open(&sandbox, "anthropic/claude-sonnet-4-5");
    let has = |models: &Value, provider: &str| refs(models).iter().any(|r| r.starts_with(&format!("{provider}/")));
    assert!(!has(&actor.call(Op::AvailableModels).unwrap().unwrap(), "groq"));
    let auth = sandbox.agent_dir.join("auth.json");
    std::fs::write(
        &auth,
        json!({"groq": {"type": "api_key", "key": "groq-key"}}).to_string(),
    )
    .unwrap();
    assert!(has(&actor.call(Op::AvailableModels).unwrap().unwrap(), "groq"));
    std::fs::write(
        &auth,
        json!({"groq": {"type": "api_key", "key": "groq-key"}, "openai": {"type": "api_key", "key": "openai-key"}})
            .to_string(),
    )
    .unwrap();
    let selected = actor
        .call(Op::SetModel {
            provider: "openai".into(),
            model_id: "gpt-5".into(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        (&selected["provider"], &selected["id"]),
        (&json!("openai"), &json!("gpt-5"))
    );
    drop(seen);
    actor.close();
}

/// An installed provider source applies only while its guard lives.
#[test]
fn cli_extension_provider_source_is_removed_with_its_guard() {
    let sandbox = common::Sandbox::new(9);
    let source: session_core::extension_providers::Source = Arc::new(|| {
        vec![session_core::extension_providers::Registration {
            name: "guarded".into(),
            config: json!({"baseUrl": "http://127.0.0.1:9", "api": "openai-completions", "apiKey": "k", "models": [{"id": "g"}]})
                .as_object()
                .unwrap()
                .clone(),
            extension_path: "/x".into(),
        }]
    });
    let listed = || {
        session_core::lane::available_models(&env(&sandbox))
            .iter()
            .any(|m| m.provider == "guarded")
    };
    let guard = session_core::extension_providers::install(&sandbox.agent_dir, source);
    assert!(listed());
    drop(guard);
    assert!(!listed());
}
