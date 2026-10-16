//! The session lane's provider streams every API the TypeScript session does (`streamSimple` over pi-ai's API
//! registry): OpenAI Codex, Azure OpenAI, Google Vertex, Mistral, Bedrock and the Claude Code CLI as well as the
//! APIs ported first. Each `streamSimple` scenario the ai crate recorded from the TypeScript providers
//! (crates/ai/fixtures/*.json) goes through `session_core::lane::Provider` with the recorded HTTP responses: the
//! request it sends and the message it ends with must be TypeScript's. A setup failure TypeScript throws ends the
//! session's request as an error message with the thrown text. Nothing reaches the network.

use agent::StreamFn;
use ai::http::{BoxFuture, HeaderMap};
use ai::json::normalize_numbers;
use ai::{
    AbortSignal, Body, HttpRequest, HttpResponse, Model, SimpleStreamOptions, StopReason, TranscriptContext, Transport,
};
use serde_json::{Map, Value, json};
use session_core::lane::{Provider, api_supported};
use std::sync::{Arc, Mutex};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ai/fixtures");

fn fixture(text: &str) -> Value {
    serde_json::from_str(&text.replace("<fixtures>", FIXTURES)).expect("fixture parses")
}

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// The recorded responses, in order; every request is kept.
#[derive(Clone, Default)]
struct Recorded {
    responses: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for Recorded {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let next = {
            let mut responses = self.responses.lock().unwrap();
            (!responses.is_empty()).then(|| responses.remove(0))
        };
        Box::pin(async move {
            let response = next.ok_or("unexpected request")?;
            let headers: HeaderMap = match response.get("headers").and_then(Value::as_object) {
                Some(headers) => headers
                    .iter()
                    .map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap_or_default().to_string()))
                    .collect(),
                None => [("content-type".to_string(), "text/event-stream".to_string())].into(),
            };
            let chunks: Vec<Vec<u8>> = response["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|chunk| match chunk {
                    Value::String(s) => s.as_bytes().to_vec(),
                    Value::Array(bytes) => bytes.iter().map(|b| b.as_u64().unwrap() as u8).collect(),
                    other => panic!("bad chunk {other}"),
                })
                .collect();
            Ok(HttpResponse {
                status: response.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
                headers,
                body: Box::new(Chunks(chunks.into_iter())),
            })
        })
    }
}

/// The scenario's model, context and options through the session's provider: the message, and the requests sent.
fn through_provider(scenario: &Value) -> (ai::AssistantMessage, Vec<HttpRequest>) {
    let model: Model = serde_json::from_value(scenario["model"].clone()).expect("model");
    let context: TranscriptContext = serde_json::from_value(scenario["context"].clone()).expect("context");
    let mut options: SimpleStreamOptions = serde_json::from_value(scenario["options"].clone()).expect("options");
    options.base.signal = Some(AbortSignal::new());
    let transport = Recorded::default();
    *transport.responses.lock().unwrap() = scenario["responses"].as_array().cloned().unwrap_or_default();
    let provider = Provider::with_router(ai::claude_subscription::fallback::global(), Arc::new(transport.clone()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let message = runtime.block_on(provider.stream(&model, &context, &options, &mut sink));
    let requests = std::mem::take(&mut *transport.requests.lock().unwrap());
    (message, requests)
}

fn pinned(message: &ai::AssistantMessage, now: &Value) -> Value {
    let mut value = serde_json::to_value(message).unwrap();
    value["timestamp"] = now.clone();
    if let Some(Value::Array(diagnostics)) = value.get_mut("diagnostics") {
        for diagnostic in diagnostics {
            diagnostic["timestamp"] = now.clone();
        }
    }
    normalize_numbers(value)
}

/// Every `streamSimple` scenario of `fixture`: TypeScript's request (URL, method, body) and final message, or the
/// thrown setup error as the message's error.
fn differences(fixture: &Value) -> Vec<String> {
    let now = fixture["now"].clone();
    let mut diffs = Vec::new();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        if scenario["fn"] != "streamSimple" || scenario.get("abortOn").is_some_and(|a| !a.is_null()) {
            continue;
        }
        let name = scenario["name"].as_str().unwrap();
        let (message, requests) = through_provider(scenario);
        if let Some(thrown) = scenario["thrown"].as_str() {
            if message.stop_reason != StopReason::Error || message.error_message.as_deref() != Some(thrown) {
                diffs.push(format!("{name}: thrown {thrown:?}, got {:?}", message.error_message));
            }
            continue;
        }
        let expected_requests = scenario["requests"].as_array().unwrap();
        if expected_requests.len() != requests.len() {
            diffs.push(format!(
                "{name}: {} requests, sent {}",
                expected_requests.len(),
                requests.len()
            ));
        }
        for (expected, actual) in expected_requests.iter().zip(&requests) {
            let body: Value = serde_json::from_str(&actual.body).unwrap_or(Value::Null);
            let actual = json!({"url": actual.url, "method": actual.method, "body": normalize_numbers(body)});
            let expected = json!({"url": expected["url"], "method": expected["method"], "body": normalize_numbers(expected["body"].clone())});
            if serde_json::to_string(&expected).unwrap() != serde_json::to_string(&actual).unwrap() {
                diffs.push(format!("{name}: request\n  expected {expected}\n  actual   {actual}"));
            }
        }
        let expected = normalize_numbers(scenario["result"].clone());
        let actual = pinned(&message, &now);
        if expected != actual {
            diffs.push(format!("{name}: result\n  expected {expected}\n  actual   {actual}"));
        }
    }
    diffs
}

fn replay(text: &str, api: &str) {
    let fixture = fixture(text);
    let simple = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["fn"] == "streamSimple" && s["model"]["api"] == api)
        .count();
    assert!(simple > 0, "the fixture has streamSimple scenarios of {api}");
    assert!(api_supported(api), "{api} is a session API");
    let diffs = differences(&fixture);
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
}

#[test]
fn session_core_provider_streams_openai_codex_responses() {
    replay(include_str!("../../ai/fixtures/codex.json"), "openai-codex-responses");
}

#[test]
fn session_core_provider_streams_azure_openai_responses() {
    replay(include_str!("../../ai/fixtures/azure.json"), "azure-openai-responses");
}

#[test]
fn session_core_provider_streams_google_vertex() {
    replay(include_str!("../../ai/fixtures/google-vertex.json"), "google-vertex");
}

#[test]
fn session_core_provider_streams_mistral_conversations() {
    replay(include_str!("../../ai/fixtures/mistral.json"), "mistral-conversations");
}

#[test]
fn session_core_provider_streams_bedrock_converse_stream() {
    replay(
        include_str!("../../ai/fixtures/bedrock.json"),
        "bedrock-converse-stream",
    );
}

/// The Claude Code CLI API runs the `claude` process (the ai crate's fake, never the real one) with the scenario's
/// recorded reply: the message is TypeScript's.
#[test]
fn session_core_provider_streams_claude_code_cli() {
    let fixture = fixture(include_str!("../../ai/fixtures/claude-code.json"));
    assert!(api_supported("claude-code-cli"));
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "claude/result-only-text")
        .expect("the result-only scenario")
        .clone();
    let dir = tempfile::tempdir().unwrap();
    for (suffix, text) in scenario["replies"].as_object().unwrap() {
        let file = if suffix.is_empty() {
            "reply".to_string()
        } else {
            format!("reply.{suffix}")
        };
        std::fs::write(dir.path().join(file), text.as_str().unwrap()).unwrap();
    }
    let log = dir.path().join("log");
    std::fs::write(&log, "").unwrap();
    let mut env: Map<String, Value> = json!({
        "ULTRON_CLAUDE_CODE_BIN": format!("{FIXTURES}/fake-claude/claude"),
        "ULTRON_CLAUDE_CODE_WARM": "0",
        "CLAUDECODE": "1",
        "CLAUDE_CODE_SESSION_ID": "parent",
        "FAKE_CLAUDE_REPLY": dir.path().join("reply").display().to_string(),
        "FAKE_CLAUDE_LOG": log.display().to_string(),
    })
    .as_object()
    .unwrap()
    .clone();
    env.extend(scenario["env"].as_object().unwrap().clone());
    let mut scenario = scenario;
    scenario["options"]["env"] = Value::Object(env);
    let (message, requests) = through_provider(&scenario);
    assert!(requests.is_empty(), "the CLI is a process, not an HTTP request");
    let expected = normalize_numbers(scenario["result"].clone());
    assert_eq!(pinned(&message, &fixture["now"]), expected);
    // The process got the recorded TypeScript argv.
    let cli = std::fs::read_to_string(&log)
        .unwrap()
        .replace(&dir.path().display().to_string(), "<dir>")
        .replace(FIXTURES, "<fixtures>");
    assert_eq!(json!(cli), scenario["cli"]);
}
