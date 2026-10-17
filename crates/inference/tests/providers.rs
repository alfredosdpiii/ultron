//! Frames reach every provider API the session streams. Replays crates/inference/fixtures/providers.json, recorded by
//! fixtures/generate-providers.mjs from the TypeScript host (NativeRlmHost + InferenceRuntime of inference.ts) whose
//! frame lanes call pi-ai's `streamSimple` of the model's API with the runtime's system prompt and `before_payload`
//! hook: the same `rlm.infer` goes through `Inference` and crates/ai's API dispatch over real HTTP to a 127.0.0.1
//! server answering with the same canned responses (taken from the ai crate's provider fixtures), and must send the
//! same requests (method, path, JSON body: system prompt, output cap, thinking budget, response schema) and return
//! the same result. The Claude Code CLI frame runs the ai crate's fake `claude` and must hand it the same argv,
//! output cap and prompt. The fixture's capOutputTokens / withContractSchema table is replayed against `payload`.

use ai::ReqwestTransport;
use inference::Inference;
use inference::payload::{cap_output_tokens, with_contract_schema};
use kernel::{EchoHandler, HostHandler, Signal};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

const AI_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ai/fixtures");

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/providers.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A provider on 127.0.0.1: records each request, answers with the next canned response (status, headers, chunks)
/// and closes; a WebSocket upgrade is refused with 404, as the generator's server does.
#[derive(Clone, Default)]
struct Server {
    queue: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Server {
    fn start(&self) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = self.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let server = server.clone();
                std::thread::spawn(move || server.serve(stream));
            }
        });
        addr
    }

    fn serve(&self, stream: TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let url = parts.next().unwrap_or_default().to_string();
        let mut headers = HashMap::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.insert(name.trim().to_lowercase(), value.trim().to_string());
            }
        }
        let mut stream = stream;
        if headers.contains_key("upgrade") {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            return;
        }
        let mut body = Vec::new();
        if let Some(length) = headers.get("content-length") {
            body.resize(length.parse().unwrap(), 0);
            reader.read_exact(&mut body).unwrap();
        } else if headers
            .get("transfer-encoding")
            .is_some_and(|te| te.contains("chunked"))
        {
            loop {
                let mut size = String::new();
                reader.read_line(&mut size).unwrap();
                let size = usize::from_str_radix(size.trim(), 16).unwrap();
                let mut chunk = vec![0; size + 2];
                reader.read_exact(&mut chunk).unwrap();
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
        }
        // Codex sends its SSE body zstd-compressed (`Content-Encoding: zstd`), as the TS server decodes it.
        let body = ai::http::decode_wire_body(headers.get("content-encoding").map(String::as_str), &body).unwrap();
        let text = String::from_utf8_lossy(&body).into_owned();
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        self.requests
            .lock()
            .unwrap()
            .push(json!({"method": method, "url": url, "body": body}));
        let canned = {
            let mut queue = self.queue.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        let Some(canned) = canned else {
            let _ = stream.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"unexpected request\"}}",
            );
            return;
        };
        let status = canned["status"].as_u64().unwrap_or(200);
        let mut head = format!("HTTP/1.1 {status} OK\r\nconnection: close\r\n");
        match canned["headers"].as_object() {
            Some(headers) => {
                for (name, value) in headers {
                    head.push_str(&format!("{name}: {}\r\n", value.as_str().unwrap()));
                }
            }
            None => head.push_str("content-type: text/event-stream\r\n"),
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        for chunk in canned["chunks"].as_array().unwrap() {
            let bytes: Vec<u8> = match chunk {
                Value::String(text) => text.as_bytes().to_vec(),
                Value::Array(bytes) => bytes.iter().map(|b| b.as_u64().unwrap() as u8).collect(),
                other => panic!("bad chunk {other}"),
            };
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
        }
    }
}

fn substituted(value: &Value, host: &str) -> Value {
    serde_json::from_str(
        &value
            .to_string()
            .replace("<host>", host)
            .replace("<fixtures>", AI_FIXTURES),
    )
    .unwrap()
}

fn normalized(value: &Value, host: &str) -> Value {
    let text = value.to_string().replace(host, "<host>");
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("frame-") {
        out.push_str(&rest[..at + 6]);
        let tail = &rest[at + 6..];
        if tail.len() >= 36 && tail[..36].bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            out.push_str("ID");
            rest = &tail[36..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    serde_json::from_str(&out).unwrap()
}

/// One scenario's `rlm.infer` through `Inference` over real HTTP: the host's result and the requests sent.
fn run(scenario: &Value, dir: &Path) -> (Value, Vec<Value>, Option<String>) {
    let server = Server::default();
    *server.queue.lock().unwrap() = scenario["responses"].as_array().cloned().unwrap_or_default();
    let host = server.start();
    let model: ai::Model = serde_json::from_value(substituted(&scenario["model"], &host)).unwrap();
    let mut env: ai::env::ProviderEnv = serde_json::from_value(substituted(
        scenario.get("env").filter(|e| !e.is_null()).unwrap_or(&json!({})),
        &host,
    ))
    .unwrap();
    let mut log = None;
    if let Some(reply) = scenario["cliReply"].as_str() {
        let cli = dir.join("claude-DIR");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(cli.join("reply"), reply).unwrap();
        std::fs::write(cli.join("log"), "").unwrap();
        for (key, value) in [
            ("ULTRON_CLAUDE_CODE_BIN", format!("{AI_FIXTURES}/fake-claude/claude")),
            ("ULTRON_CLAUDE_CODE_WARM", "0".into()),
            ("FAKE_CLAUDE_REPLY", cli.join("reply").display().to_string()),
            ("FAKE_CLAUDE_LOG", cli.join("log").display().to_string()),
        ] {
            env.insert(key.into(), value);
        }
        log = Some(cli.join("log"));
    }
    let handler = Inference::new(
        Arc::new(EchoHandler),
        model,
        scenario["apiKey"].as_str().map(str::to_string),
        Arc::new(ReqwestTransport::new(None)),
        dir.join("context"),
    )
    .with_provider_env(env);
    let Value::Object(payload) = scenario["payload"].clone() else {
        panic!("payload")
    };
    let result = handler.handle("rlm.infer", payload, &Signal::new()).unwrap();
    let requests = server.requests.lock().unwrap().clone();
    let cli = log.map(|log| {
        std::fs::read_to_string(log)
            .unwrap()
            .replace(&dir.display().to_string(), "<dir>")
            .replace(AI_FIXTURES, "<fixtures>")
    });
    let requests = requests.iter().map(|r| normalized(r, &host)).collect();
    (normalized(&result, &host), requests, cli)
}

fn replay(name: &str) {
    let fixture = fixture();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"));
    let dir = tempfile::tempdir().unwrap();
    let (result, requests, cli) = run(scenario, dir.path());
    assert_eq!(result, scenario["result"], "{name}: rlm.infer result");
    match cli {
        Some(cli) => assert_eq!(json!(cli), scenario["cli"], "{name}: the fake claude's argv and stdin"),
        None => {
            let expected = scenario["requests"].as_array().unwrap();
            assert_eq!(requests.len(), expected.len(), "{name}: requests {requests:#?}");
            for (actual, expected) in requests.iter().zip(expected) {
                assert_eq!(actual["method"], expected["method"], "{name}: method");
                assert_eq!(actual["url"], expected["url"], "{name}: path");
                assert_eq!(
                    ai::json::normalize_numbers(actual["body"].clone()).to_string(),
                    ai::json::normalize_numbers(expected["body"].clone()).to_string(),
                    "{name}: body"
                );
            }
        }
    }
}

#[test]
fn inference_frames_call_anthropic_messages_like_typescript() {
    replay("anthropic-messages");
}

#[test]
fn inference_frames_cap_anthropic_thinking_budgets_like_typescript() {
    replay("anthropic-messages-thinking");
}

#[test]
fn inference_frames_call_openai_completions_like_typescript() {
    replay("openai-completions");
}

#[test]
fn inference_frames_call_openai_responses_like_typescript() {
    replay("openai-responses");
}

#[test]
fn inference_frames_call_azure_openai_responses_like_typescript() {
    replay("azure-openai-responses");
}

#[test]
fn inference_frames_call_openai_codex_responses_like_typescript() {
    replay("openai-codex-responses");
}

#[test]
fn inference_frames_call_google_generative_ai_like_typescript() {
    replay("google-generative-ai");
}

#[test]
fn inference_frames_call_google_vertex_like_typescript() {
    replay("google-vertex");
}

#[test]
fn inference_frames_call_mistral_conversations_like_typescript() {
    replay("mistral-conversations");
}

#[test]
fn inference_frames_call_bedrock_converse_stream_like_typescript() {
    replay("bedrock-converse-stream");
}

#[test]
fn inference_frames_call_pi_messages_like_typescript() {
    replay("pi-messages");
}

#[test]
fn inference_frames_hand_the_contract_to_the_claude_code_cli_like_typescript() {
    replay("claude-code-cli");
}

/// Every scenario of the fixture has a test above.
#[test]
fn inference_frames_every_recorded_provider_scenario_is_replayed() {
    let names: Vec<String> = fixture()["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    let source = include_str!("providers.rs");
    for name in names {
        assert!(
            source.contains(&format!("replay(\"{name}\")")),
            "{name} is not replayed"
        );
    }
}

#[test]
fn inference_frame_payload_caps_match_typescript() {
    for case in fixture()["caps"].as_array().unwrap() {
        let api = case["api"].as_str().unwrap();
        let contract = Some(&case["contract"]).filter(|c| !c.is_null());
        let out = cap_output_tokens(
            with_contract_schema(case["payload"].clone(), contract, api),
            case["limit"].as_u64().unwrap(),
            api,
            case["provider"].as_str().unwrap(),
        );
        assert_eq!(
            ai::json::normalize_numbers(out),
            ai::json::normalize_numbers(case["out"].clone()),
            "{case}"
        );
    }
}
