//! Custom models.json providers through the `ultron` binary:
//!
//! - `--list-models` replays the listing (stdout) and the models.json warning (stderr) the TypeScript `listModels`
//!   printed for each case of `crates/ai/fixtures/model-registry.json` (`generate-model-registry.mjs`);
//! - `ultron -p --model <custom>/<id>` reaches a loopback OpenAI-compatible stub defined only in the test's
//!   models.json, with the model id, the configured key (`$VAR`) and the provider and model headers (`!command`,
//!   literal), and prints the stub's answer; a key that does not resolve fails before any request.
//!
//! Every process runs with private HOME, ULTRON_CODING_AGENT_DIR and ULTRON_SERVER_DIR; nothing contacts a provider.

#[path = "daemon_common/mod.rs"]
mod common;

use cli::rpc::client::{RpcClient, RpcClientOptions};
use common::{HANG_GUARD, assert_no_processes};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn registry_fixture() -> Value {
    serde_json::from_str(include_str!("../../ai/fixtures/model-registry.json")).unwrap()
}

fn docs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/coding-agent/docs")
        .canonicalize()
        .unwrap()
}

/// A private root with `agent/`, `cwd/` and `server/` and the given models.json.
struct Root {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Root {
    fn new(models: &Value) -> Root {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("agent")).unwrap();
        std::fs::create_dir_all(root.join("cwd")).unwrap();
        std::fs::write(
            root.join("agent/models.json"),
            serde_json::to_string_pretty(models).unwrap(),
        )
        .unwrap();
        Root { _tmp: tmp, root }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
        command
            .args(args)
            .env_clear()
            .current_dir(self.root.join("cwd"))
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("ULTRON_CODING_AGENT_DIR", self.root.join("agent"))
            .env("ULTRON_SERVER_DIR", self.root.join("server"))
            .env("NO_COLOR", "1")
            .env("TZ", "UTC");
        for (key, value) in env {
            command.env(key, value);
        }
        command
    }
}

impl Drop for Root {
    /// Nothing the test started outlives it.
    fn drop(&mut self) {
        if !std::thread::panicking() {
            assert_no_processes(&self.root, HANG_GUARD);
        }
    }
}

#[test]
fn cli_custom_providers_list_models_matches_typescript() {
    let docs = docs().display().to_string();
    for case in registry_fixture()["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let root = Root::new(&case["models"]);
        let env: Vec<(String, String)> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let search = case["list"]["search"].as_str().unwrap();
        let output = root.command(&["--list-models", search], &env).output().unwrap();
        let agent = root.root.join("agent").display().to_string();
        let stdout = String::from_utf8(output.stdout).unwrap().replace(&docs, "<docs>");
        let stderr = String::from_utf8(output.stderr).unwrap().replace(&agent, "<dir>");
        assert_eq!(stdout, case["list"]["stdout"].as_str().unwrap(), "{name}: stdout");
        assert_eq!(stderr, case["list"]["stderr"].as_str().unwrap(), "{name}: stderr");
        assert_eq!(output.status.code(), Some(0), "{name}: exit code");
    }
}

/// What the stub saw of one request.
#[derive(Clone, Debug)]
struct Seen {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// An OpenAI chat-completions SSE reply with `text`.
fn completions_text(text: &str) -> String {
    let chunk = |delta: Value, finish: Value, usage: Option<Value>| {
        let mut c = json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "gpt-6.1-sol", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
        if let Some(u) = usage {
            c["usage"] = u;
        }
        format!("data: {c}\n\n")
    };
    format!(
        "{}{}{}data: [DONE]\n\n",
        chunk(json!({"role": "assistant", "content": ""}), Value::Null, None),
        chunk(json!({"content": text}), Value::Null, None),
        chunk(
            json!({}),
            json!("stop"),
            Some(json!({"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}))
        ),
    )
}

/// A loopback OpenAI-compatible server answering every request with `reply`, recording each.
fn stub(reply: String) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let url = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut headers = Vec::new();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap();
                    }
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
            shared.lock().unwrap().push(Seen { url, headers, body });
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (port, seen)
}

/// models.json defining only the stub's provider: the key from `$STUB_KEY`, a provider header from a command, a
/// literal one, and a model header.
fn stub_models(port: u16) -> Value {
    json!({"providers": {"stubproxy": {
        "baseUrl": format!("http://127.0.0.1:{port}/v1"),
        "api": "openai-completions",
        "apiKey": "$STUB_KEY",
        "headers": {"x-proxy-token": "!printf proxy-secret", "x-static": "static"},
        "compat": {"supportsStore": false, "supportsDeveloperRole": false},
        "models": [
            {"id": "gpt-6.1-sol", "name": "Sol", "contextWindow": 200000, "maxTokens": 4096, "headers": {"x-model": "sol"}},
            {"id": "plain"},
        ],
    }}})
}

#[test]
fn cli_custom_provider_print_mode_reaches_loopback_stub() {
    let (port, seen) = stub(completions_text("Hello from the custom stub"));
    let root = Root::new(&stub_models(port));
    let output = root
        .command(
            &["-p", "--no-session", "--model", "stubproxy/gpt-6.1-sol", "say hi"],
            &[("STUB_KEY", "stub-key-123"), ("ULTRON_AUTO_NAME", "off")],
        )
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Hello from the custom stub\n");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let request = &seen[0];
    assert_eq!(request.url, "/v1/chat/completions");
    assert_eq!(request.body["model"], "gpt-6.1-sol");
    assert_eq!(request.header("authorization"), Some("Bearer stub-key-123"));
    assert_eq!(request.header("x-proxy-token"), Some("proxy-secret"));
    assert_eq!(request.header("x-static"), Some("static"));
    assert_eq!(request.header("x-model"), Some("sol"));
    // The model's tool list stays exactly [rlm].
    let tools: Vec<&str> = request.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["rlm"]);
    // models.json's maxTokens reaches the request.
    assert_eq!(request.body["max_completion_tokens"], 4096);
}

#[test]
fn cli_custom_provider_unresolved_key_fails_before_any_request() {
    let (port, seen) = stub(completions_text("unused"));
    let root = Root::new(&stub_models(port));
    let output = root
        .command(
            &["-p", "--no-session", "--model", "stubproxy/gpt-6.1-sol", "say hi"],
            &[],
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Error: API key auth failed for provider stubproxy: Failed to resolve API key for provider \"stubproxy\" from environment variable: STUB_KEY\n"
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// RPC `get_available_models` lists the custom models (never their resolved secrets), `set_model` selects one, and
/// the next prompt goes to the stub with that model's id and headers.
#[test]
fn cli_custom_provider_rpc_lists_and_selects_custom_models() {
    let (port, seen) = stub(completions_text("Answer from sol"));
    let root = Root::new(&stub_models(port));
    let env = [
        ("PATH", "/usr/bin:/bin".to_string()),
        ("HOME", root.root.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR", root.root.join("agent").display().to_string()),
        ("ULTRON_SERVER_DIR", root.root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL", "off".into()),
        ("PI_OFFLINE", "1".into()),
        ("STUB_KEY", "stub-key-123".into()),
        ("NO_COLOR", "1".into()),
        ("TZ", "UTC".into()),
    ];
    let options = RpcClientOptions {
        program: env!("CARGO_BIN_EXE_ultron").into(),
        cwd: Some(root.root.join("cwd")),
        env_clear: true,
        env: env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        args: ["--mode", "rpc", "--model", "stubproxy/plain"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    };
    let mut client = RpcClient::start(&options).unwrap();
    let available = client.send(json!({"type": "get_available_models"})).unwrap();
    let models = RpcClient::data(&available).unwrap()["models"].clone();
    let ids: Vec<String> = models
        .as_array()
        .unwrap()
        .iter()
        .map(|m| format!("{}/{}", m["provider"].as_str().unwrap(), m["id"].as_str().unwrap()))
        .collect();
    assert_eq!(ids, ["stubproxy/gpt-6.1-sol", "stubproxy/plain"]);
    assert_eq!(models[0]["maxTokens"], 4096);
    let listed = models.to_string();
    assert!(
        !listed.contains("proxy-secret") && !listed.contains("stub-key-123"),
        "{listed}"
    );
    let selected = client
        .send(json!({"type": "set_model", "provider": "stubproxy", "modelId": "gpt-6.1-sol"}))
        .unwrap();
    assert_eq!(RpcClient::data(&selected).unwrap()["id"], "gpt-6.1-sol");
    assert!(!selected.to_string().contains("proxy-secret"));
    client.prompt_and_wait("say hi", HANG_GUARD).unwrap();
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("Answer from sol")
    );
    let (_, code) = client.finish(HANG_GUARD);
    assert_eq!(code, Some(0));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].body["model"], "gpt-6.1-sol");
    assert_eq!(seen[0].header("authorization"), Some("Bearer stub-key-123"));
    assert_eq!(seen[0].header("x-proxy-token"), Some("proxy-secret"));
    assert_eq!(seen[0].header("x-model"), Some("sol"));
}

/// A key saved in auth.json (`/login` or by hand) is the key `ultron -p` sends: it owns its provider over the
/// models.json `apiKey` and the environment (`ModelRuntime.getAuth`, `fixtures/request-key.json` in session-core),
/// and `--api-key` still beats it.
#[test]
fn cli_print_mode_sends_the_auth_json_key() {
    let (port, seen) = stub(completions_text("Hello with the stored key"));
    let root = Root::new(&stub_models(port));
    std::fs::write(
        root.root.join("agent/auth.json"),
        r#"{"stubproxy": {"type": "api_key", "key": "stored-fake-key"}}"#,
    )
    .unwrap();
    let run = |args: &[&str], env: &[(&str, &str)]| {
        let mut argv = vec!["-p", "--no-session", "--model", "stubproxy/gpt-6.1-sol"];
        argv.extend_from_slice(args);
        argv.push("say hi");
        let mut vars = vec![("ULTRON_AUTO_NAME", "off")];
        vars.extend_from_slice(env);
        let output = root.command(&argv, &vars).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "Hello with the stored key\n");
    };
    // Only the stored key exists: before the port, the run failed with "API key auth failed" for `$STUB_KEY`.
    run(&[], &[]);
    // The stored key beats the models.json key the environment would resolve.
    run(&[], &[("STUB_KEY", "json-env-key")]);
    // `--api-key` beats the stored key.
    run(&["--api-key", "flag-fake-key"], &[("STUB_KEY", "json-env-key")]);
    let seen = seen.lock().unwrap().clone();
    let keys: Vec<Option<&str>> = seen.iter().map(|s| s.header("authorization")).collect();
    assert_eq!(
        keys,
        [
            Some("Bearer stored-fake-key"),
            Some("Bearer stored-fake-key"),
            Some("Bearer flag-fake-key")
        ]
    );
}

/// An Anthropic Messages SSE reply with `text`.
fn messages_text(text: &str) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "k3", "content": [], "stop_reason": null, "usage": {"input_tokens": 5, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// Kimi For Coding signed in with OAuth: `ultron -p` passes the preflight with no API key (the credential is a
/// header, `toAuth`'s `Authorization: Bearer <access>`) and the request carries that header, not `x-api-key`. The
/// provider's base URL points at a loopback stub; any other host goes to a closed loopback proxy port.
#[test]
fn cli_kimi_oauth_print_mode_sends_the_bearer_header() {
    let (port, seen) = stub(messages_text("Hello from Kimi"));
    let root =
        Root::new(&json!({"providers": {"kimi-coding": {"baseUrl": format!("http://127.0.0.1:{port}/coding")}}}));
    std::fs::write(
        root.root.join("agent/auth.json"),
        json!({"kimi-coding": {"type": "oauth", "access": "kimi-access", "refresh": "kimi-refresh", "expires": 4102444800000_i64}})
            .to_string(),
    )
    .unwrap();
    let output = root
        .command(
            &["-p", "--no-session", "--model", "kimi-coding/k3", "say hi"],
            &[
                ("ULTRON_AUTO_NAME", "off"),
                ("PI_OFFLINE", "1"),
                ("HTTP_PROXY", "http://127.0.0.1:9"),
                ("HTTPS_PROXY", "http://127.0.0.1:9"),
                ("NO_PROXY", "127.0.0.1,localhost"),
            ],
        )
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Hello from Kimi\n");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].url, "/coding/v1/messages?beta=true");
    assert_eq!(seen[0].header("authorization"), Some("Bearer kimi-access"));
    assert_eq!(seen[0].header("x-api-key"), None);
}
