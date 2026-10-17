//! Provider auth that reaches the wire through the `ultron` binary (print mode against loopback stubs):
//! `ANTHROPIC_AUTH_TOKEN` as `Authorization: Bearer` with no `x-api-key` (providers/anthropic.ts), and the Cloudflare
//! AI Gateway's stored /login credential: its account and gateway ids fill the base URL and its key goes out as
//! `cf-aig-authorization` (providers/cloudflare-auth.ts, `applyAuth`'s request env).
//!
//! Every process runs with private HOME, ULTRON_CODING_AGENT_DIR and ULTRON_SERVER_DIR; any non-loopback host goes
//! to a closed loopback proxy port; nothing contacts a provider.

#[path = "daemon_common/mod.rs"]
mod common;

use common::{HANG_GUARD, assert_no_processes};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

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

/// What the stub saw of one request.
#[derive(Clone, Debug)]
struct Seen {
    url: String,
    headers: Vec<(String, String)>,
    #[allow(dead_code)]
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
    stub_with("200 OK", "text/event-stream", reply)
}

/// A loopback server answering every request with `status` and a `content_type` body, recording each.
fn stub_with(status: &'static str, content_type: &'static str, reply: String) -> (u16, Arc<Mutex<Vec<Seen>>>) {
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
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (port, seen)
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

const OFFLINE: [(&str, &str); 5] = [
    ("ULTRON_AUTO_NAME", "off"),
    ("PI_OFFLINE", "1"),
    ("HTTP_PROXY", "http://127.0.0.1:9"),
    ("HTTPS_PROXY", "http://127.0.0.1:9"),
    ("NO_PROXY", "127.0.0.1,localhost"),
];

fn print(root: &Root, model: &str, env: &[(&str, &str)]) -> String {
    let mut vars = OFFLINE.to_vec();
    vars.extend_from_slice(env);
    let output = root
        .command(&["-p", "--no-session", "--model", model, "say hi"], &vars)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `ANTHROPIC_AUTH_TOKEN` beside `ANTHROPIC_API_KEY`: `ultron -p` passes the preflight and the request carries
/// `Authorization: Bearer <token>` and no `x-api-key` (TypeScript: request-auth-env.json case 0); a stored key wins
/// over the token.
#[test]
fn cli_anthropic_auth_token_print_mode_sends_the_bearer_header() {
    let (port, seen) = stub(messages_text("Hello with the token"));
    let root = Root::new(&json!({"providers": {"anthropic": {"baseUrl": format!("http://127.0.0.1:{port}")}}}));
    let env = [("ANTHROPIC_AUTH_TOKEN", "tok-fake"), ("ANTHROPIC_API_KEY", "sk-fake")];
    assert_eq!(
        print(&root, "anthropic/claude-haiku-4-5", &env),
        "Hello with the token\n"
    );
    std::fs::write(
        root.root.join("agent/auth.json"),
        r#"{"anthropic": {"type": "api_key", "key": "sk-stored-fake"}}"#,
    )
    .unwrap();
    assert_eq!(
        print(&root, "anthropic/claude-haiku-4-5", &env),
        "Hello with the token\n"
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0].url, "/v1/messages?beta=true");
    assert_eq!(seen[0].header("authorization"), Some("Bearer tok-fake"));
    assert_eq!(seen[0].header("x-api-key"), None);
    assert_eq!(seen[1].header("authorization"), None);
    assert_eq!(seen[1].header("x-api-key"), Some("sk-stored-fake"));
}

/// The AI Gateway signed in with /login (key, account id, gateway id in auth.json): the request goes to the base URL
/// with both ids filled in and authenticates by `cf-aig-authorization` alone, with no API key in the environment.
#[test]
fn cli_cloudflare_gateway_login_fills_the_base_url_and_sends_cf_aig_authorization() {
    let (port, seen) = stub(completions_text("Hello through the gateway"));
    let base = format!("http://127.0.0.1:{port}/v1/{{CLOUDFLARE_ACCOUNT_ID}}/{{CLOUDFLARE_GATEWAY_ID}}/compat");
    let root = Root::new(&json!({"providers": {"cloudflare-ai-gateway": {"baseUrl": base}}}));
    std::fs::write(
        root.root.join("agent/auth.json"),
        json!({"cloudflare-ai-gateway": {"type": "api_key", "key": "cf-fake", "env": {"CLOUDFLARE_ACCOUNT_ID": "acct-1", "CLOUDFLARE_GATEWAY_ID": "gw-1"}}})
            .to_string(),
    )
    .unwrap();
    let model = "cloudflare-ai-gateway/workers-ai/@cf/deepseek-ai/deepseek-v4-flash-0731";
    assert_eq!(print(&root, model, &[]), "Hello through the gateway\n");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].url, "/v1/acct-1/gw-1/compat/chat/completions");
    assert_eq!(seen[0].header("cf-aig-authorization"), Some("Bearer cf-fake"));
    assert_eq!(seen[0].header("authorization"), None);
    assert_eq!(seen[0].header("x-api-key"), None);
}

/// Workers AI with the key and account id in the environment only: the account id fills the base URL.
#[test]
fn cli_cloudflare_workers_ai_env_account_fills_the_base_url() {
    let (port, seen) = stub(completions_text("Hello from Workers AI"));
    let base = format!("http://127.0.0.1:{port}/client/v4/accounts/{{CLOUDFLARE_ACCOUNT_ID}}/ai/v1");
    let root = Root::new(&json!({"providers": {"cloudflare-workers-ai": {"baseUrl": base}}}));
    let env = [
        ("CLOUDFLARE_API_KEY", "cf-env-fake"),
        ("CLOUDFLARE_ACCOUNT_ID", "acct-env"),
    ];
    let model = "cloudflare-workers-ai/@cf/deepseek-ai/deepseek-v4-flash-0731";
    assert_eq!(print(&root, model, &env), "Hello from Workers AI\n");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].url, "/client/v4/accounts/acct-env/ai/v1/chat/completions");
    assert_eq!(seen[0].header("authorization"), Some("Bearer cf-env-fake"));
}

/// The cases of `crates/ai/fixtures/provider-headers.json` (TypeScript, `generate-provider-headers.mjs`) that set
/// `OPENAI_ORG_ID`/`OPENAI_PROJECT_ID`: the binary's process environment, as the OpenAI SDK reads it, adds
/// `OpenAI-Organization`/`OpenAI-Project` to OpenAI completions, responses and Azure requests (trimmed; blank is
/// unset), never to Anthropic's. The provider's base URL points at the stub; the caller-header case is replayed as a
/// models.json header, which the SDK also sends over its own.
#[test]
fn cli_openai_organization_and_project_headers_match_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../../ai/fixtures/provider-headers.json")).unwrap();
    let mut replayed = 0;
    for spec in fixture["cases"].as_array().unwrap() {
        let name = spec["name"].as_str().unwrap();
        let vars = spec["env"].as_object().unwrap();
        if !vars.contains_key("OPENAI_ORG_ID") {
            continue;
        }
        replayed += 1;
        // A 400 is final: no retries, the run fails at once after one request.
        let (port, seen) = stub_with(
            "400 Bad Request",
            "application/json",
            json!({"error": {"message": "recorded"}}).to_string(),
        );
        let (provider, _) = spec["model"].as_str().unwrap().split_once('/').unwrap();
        let base = match provider {
            "anthropic" => format!("http://127.0.0.1:{port}"),
            _ => format!("http://127.0.0.1:{port}/v1"),
        };
        let mut config = json!({"baseUrl": base});
        if let Some(headers) = spec["options"]["headers"].as_object() {
            config["headers"] = Value::Object(headers.clone());
        }
        let models = match provider {
            "azure-openai-responses" => json!({}),
            _ => json!({"providers": {provider: config}}),
        };
        let root = Root::new(&models);
        let mut env: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        if provider == "azure-openai-responses" {
            env.retain(|(k, _)| k != "AZURE_OPENAI_BASE_URL");
            env.push((
                "AZURE_OPENAI_BASE_URL".into(),
                format!("http://127.0.0.1:{port}/openai/v1"),
            ));
        }
        let mut env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        env.extend_from_slice(&OFFLINE);
        let model = spec["model"].as_str().unwrap();
        let output = root
            .command(&["-p", "--no-session", "--model", model, "say hi"], &env)
            .output()
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        let first = seen
            .first()
            .unwrap_or_else(|| panic!("{name}: no request; {}", String::from_utf8_lossy(&output.stderr)));
        let headers: serde_json::Map<String, Value> = ["x-opencode-session", "openai-organization", "openai-project"]
            .into_iter()
            .filter_map(|n| Some((n.to_string(), json!(first.header(n)?))))
            .collect();
        assert_eq!(Value::Object(headers), spec["request"]["headers"], "{name}");
        let path = spec["request"]["url"].as_str().unwrap().split_once("/v1").unwrap().1;
        assert!(first.url.ends_with(path), "{name}: {} vs {path}", first.url);
    }
    assert_eq!(replayed, 6);
}

/// `crates/ai/fixtures/bedrock-endpoint.json` (TypeScript over the AWS SDK, `generate-bedrock-endpoint.mjs`) through
/// the binary: with a configured region the SDK's configured endpoint applies (`AWS_ENDPOINT_URL_BEDROCK_RUNTIME`,
/// `AWS_ENDPOINT_URL`, the profile's services section and `endpoint_url`, `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS`), read
/// from the process environment and the shared config files; a custom base URL, or no region, keeps the model's
/// endpoint. Loopback endpoints reach the stub directly; an AWS endpoint reaches it as the HTTPS proxy's `CONNECT`.
#[test]
fn cli_bedrock_configured_endpoint_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../../ai/fixtures/bedrock-endpoint.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 17);
    for spec in cases {
        let name = spec["name"].as_str().unwrap();
        let (port, seen) = stub_with(
            "400 Bad Request",
            "application/json",
            json!({"message": "recorded"}).to_string(),
        );
        let fill = |text: &str| text.replace("<port>", &port.to_string());
        let base = fill(spec["baseUrl"].as_str().unwrap());
        let root = Root::new(&json!({"providers": {"amazon-bedrock": {"baseUrl": base}}}));
        std::fs::write(
            root.root.join("aws-config"),
            fill(spec["config"].as_str().unwrap_or("")),
        )
        .unwrap();
        std::fs::write(
            root.root.join("aws-credentials"),
            fill(spec["credentials"].as_str().unwrap_or("")),
        )
        .unwrap();
        let config = root.root.join("aws-config").display().to_string();
        let credentials = root.root.join("aws-credentials").display().to_string();
        let proxy = format!("http://127.0.0.1:{port}");
        let mut env: Vec<(String, String)> = spec["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), fill(v.as_str().unwrap())))
            .collect();
        for (k, v) in [
            ("AWS_CONFIG_FILE", config.as_str()),
            ("AWS_SHARED_CREDENTIALS_FILE", credentials.as_str()),
            ("AWS_BEDROCK_SKIP_AUTH", "1"),
            ("AWS_ACCESS_KEY_ID", "AKIDFAKE"),
            ("AWS_SECRET_ACCESS_KEY", "secret-fake"),
            ("ULTRON_AUTO_NAME", "off"),
            ("PI_OFFLINE", "1"),
            ("HTTP_PROXY", proxy.as_str()),
            ("HTTPS_PROXY", proxy.as_str()),
            ("NO_PROXY", "127.0.0.1,localhost"),
        ] {
            env.push((k.into(), v.into()));
        }
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let output = root
            .command(
                &[
                    "-p",
                    "--no-session",
                    "--model",
                    "amazon-bedrock/amazon.nova-lite-v1:0",
                    "say hi",
                ],
                &env,
            )
            .output()
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        let first = seen
            .first()
            .unwrap_or_else(|| panic!("{name}: no request; {}", String::from_utf8_lossy(&output.stderr)));
        let url = spec["request"]["url"].as_str().unwrap();
        let expected = match url.strip_prefix("https://") {
            Some(rest) => format!("{}:443", rest.split('/').next().unwrap()),
            None => url.strip_prefix("http://127.0.0.1:<port>").unwrap().to_string(),
        };
        assert_eq!(first.url, expected, "{name}");
    }
}
