//! Replays `fixtures/providers.json` (recorded from the TypeScript providers by `fixtures/generate.mjs`): the same canned
//! HTTP/SSE responses go through the Rust providers, which must send the same requests (URL, method, headers minus SDK
//! telemetry, JSON body including key order), emit the same events, and return the same final message.

use ai::anthropic::{self, AnthropicOptions};
use ai::http::{BoxFuture, HeaderMap};
use ai::json::normalize_numbers;
use ai::openai_completions::{self, OpenAICompletionsOptions};
use ai::openai_responses::{self, OpenAIResponsesOptions};
use ai::options::pi_user_agent;
use ai::{
    AbortSignal, AssistantMessageEvent, Body, HttpRequest, HttpResponse, Model, SimpleStreamOptions, TranscriptContext,
    Transport,
};
use serde_json::{Map, Value, json};
use std::sync::{Arc, LazyLock, Mutex};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/providers.json")).expect("fixture parses"));

struct Canned {
    status: u16,
    headers: HeaderMap,
    chunks: Vec<Vec<u8>>,
}

struct FixtureBody(std::vec::IntoIter<Vec<u8>>);

impl Body for FixtureBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

#[derive(Clone, Default)]
struct FixtureTransport {
    queue: Arc<Mutex<Vec<Canned>>>,
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for FixtureTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let canned = {
            let mut queue = self.queue.lock().unwrap();
            if queue.is_empty() { None } else { Some(queue.remove(0)) }
        };
        Box::pin(async move {
            let canned = canned.ok_or("unexpected request")?;
            Ok(HttpResponse {
                status: canned.status,
                headers: canned.headers,
                body: Box::new(FixtureBody(canned.chunks.into_iter())),
            })
        })
    }
}

fn canned(response: &Value) -> Canned {
    let headers = match response.get("headers").and_then(Value::as_object) {
        Some(headers) => headers
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap().to_string()))
            .collect(),
        None => [("content-type".to_string(), "text/event-stream".to_string())].into(),
    };
    let chunks = response["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|chunk| match chunk {
            Value::String(s) => s.as_bytes().to_vec(),
            Value::Array(bytes) => bytes.iter().map(|b| b.as_u64().unwrap() as u8).collect(),
            other => panic!("bad chunk {other}"),
        })
        .collect();
    Canned {
        status: response.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
        headers,
        chunks,
    }
}

const SCRATCH_KEYS: &[&str] = &["index", "partialJson", "partialArgs", "customInput", "streamIndex"];

/// Upstream's streaming scratch fields leak into `partial` snapshots; the Rust partials do not carry them.
fn strip_scratch(event: &mut Value) {
    for key in ["partial", "message", "error"] {
        if let Some(Value::Array(content)) = event.get_mut(key).and_then(|m| m.get_mut("content")) {
            for block in content {
                if let Some(block) = block.as_object_mut() {
                    for scratch in SCRATCH_KEYS {
                        block.shift_remove(*scratch);
                    }
                }
            }
        }
    }
}

/// Wall-clock fields: the generator pins `Date.now()`; Rust reads the clock.
fn pin_clock(message: &mut Value, now: &Value) {
    if let Some(object) = message.as_object_mut() {
        object.insert("timestamp".into(), now.clone());
        if let Some(Value::Array(diagnostics)) = object.get_mut("diagnostics") {
            for diagnostic in diagnostics {
                diagnostic["timestamp"] = now.clone();
            }
        }
    }
}

fn pin_event_clock(event: &mut Value, now: &Value) {
    for key in ["partial", "message", "error"] {
        if let Some(message) = event.get_mut(key) {
            pin_clock(message, now);
        }
    }
}

struct Outcome {
    requests: Vec<Value>,
    events: Vec<Value>,
    result: Value,
    thrown: Value,
}

fn run(scenario: &Value) -> Outcome {
    let model: Model = serde_json::from_value(scenario["model"].clone()).expect("model");
    let context: TranscriptContext = serde_json::from_value(scenario["context"].clone()).expect("context");
    let transport = FixtureTransport::default();
    *transport.queue.lock().unwrap() = scenario["responses"].as_array().unwrap().iter().map(canned).collect();
    let signal = AbortSignal::new();
    let abort_on = scenario
        .get("abortOn")
        .filter(|a| !a.is_null())
        .map(|a| (a["type"].as_str().unwrap().to_string(), a["nth"].as_u64().unwrap()));
    let patch = scenario.get("payloadPatch").and_then(Value::as_object).cloned();
    let on_payload: Option<ai::options::PayloadHook> = patch.map(|patch| {
        Arc::new(move |payload: &Value, _: &Model| {
            let mut merged = payload.as_object().cloned().unwrap_or_default();
            merged.extend(patch.clone());
            Some(Value::Object(merged))
        }) as ai::options::PayloadHook
    });
    let mut events = Vec::new();
    let mut seen = 0;
    let trigger = signal.clone();
    let mut sink = |event: &AssistantMessageEvent| {
        let value = serde_json::to_value(event).unwrap();
        if let Some((kind, nth)) = &abort_on
            && value["type"] == kind.as_str()
        {
            seen += 1;
            if seen == *nth {
                trigger.abort();
            }
        }
        events.push(value);
    };
    let options = scenario["options"].clone();
    let simple = scenario["fn"] == "streamSimple";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let outcome: Result<ai::AssistantMessage, String> = runtime.block_on(async {
        macro_rules! with_base {
            ($ty:ty) => {{
                let mut parsed: $ty = serde_json::from_value(options.clone()).expect("options");
                parsed.base.signal = Some(signal.clone());
                parsed.base.on_payload = on_payload.clone();
                parsed
            }};
        }
        match (model.api.as_str(), simple) {
            ("anthropic-messages", false) => {
                Ok(anthropic::stream(&model, &context, &with_base!(AnthropicOptions), &transport, &mut sink).await)
            }
            ("openai-completions", false) => Ok(openai_completions::stream(
                &model,
                &context,
                &with_base!(OpenAICompletionsOptions),
                &transport,
                &mut sink,
            )
            .await),
            ("openai-responses", false) => Ok(openai_responses::stream(
                &model,
                &context,
                &with_base!(OpenAIResponsesOptions),
                &transport,
                &mut sink,
            )
            .await),
            ("anthropic-messages", true) => {
                anthropic::stream_simple(
                    &model,
                    &context,
                    &with_base!(SimpleStreamOptions),
                    &transport,
                    &mut sink,
                )
                .await
            }
            ("openai-completions", true) => {
                openai_completions::stream_simple(
                    &model,
                    &context,
                    &with_base!(SimpleStreamOptions),
                    &transport,
                    &mut sink,
                )
                .await
            }
            ("openai-responses", true) => {
                openai_responses::stream_simple(
                    &model,
                    &context,
                    &with_base!(SimpleStreamOptions),
                    &transport,
                    &mut sink,
                )
                .await
            }
            (api, _) => panic!("unknown api {api}"),
        }
    });
    let user_agent = pi_user_agent();
    let requests = transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            let headers: Map<String, Value> = r
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), json!(if k == "user-agent" && *v == user_agent { "<pi-user-agent>" } else { v.as_str() })))
                .collect();
            json!({"url": r.url, "method": r.method, "headers": headers, "body": serde_json::from_str::<Value>(&r.body).unwrap()})
        })
        .collect();
    let (result, thrown) = match outcome {
        Ok(message) => (serde_json::to_value(message).unwrap(), Value::Null),
        Err(message) => (Value::Null, json!(message)),
    };
    Outcome {
        requests,
        events,
        result,
        thrown,
    }
}

fn compact(value: &Value) -> String {
    serde_json::to_string(&normalize_numbers(value.clone())).unwrap()
}

/// Every difference between the recorded and the Rust run of `scenario`, empty when they match exactly.
fn differences(scenario: &Value) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let now = FIXTURE["now"].clone();
    let rust = run(scenario);
    let mut diffs = Vec::new();
    // Request parts compare as text (key order included); messages compare as JSON values, because upstream adds
    // optional message fields in whatever order the stream sets them.
    let mut check = |what: &str, expected: &Value, actual: &Value| {
        let ordered = what.starts_with("request");
        let (e, a) = (normalize_numbers(expected.clone()), normalize_numbers(actual.clone()));
        if (ordered && compact(&e) != compact(&a)) || (!ordered && e != a) {
            diffs.push(format!(
                "{name}: {what}\n  expected {}\n  actual   {}",
                compact(&e),
                compact(&a)
            ));
        }
    };
    let expected_requests = scenario["requests"].as_array().unwrap();
    check(
        "request count",
        &json!(expected_requests.len()),
        &json!(rust.requests.len()),
    );
    for (i, (expected, actual)) in expected_requests.iter().zip(&rust.requests).enumerate() {
        for part in ["url", "method", "headers", "body"] {
            check(&format!("request {i} {part}"), &expected[part], &actual[part]);
        }
    }
    let expected_events = scenario["events"].as_array().unwrap();
    check(
        "event types",
        &json!(expected_events.iter().map(|e| e["type"].clone()).collect::<Vec<_>>()),
        &json!(rust.events.iter().map(|e| e["type"].clone()).collect::<Vec<_>>()),
    );
    for (i, (expected, actual)) in expected_events.iter().zip(&rust.events).enumerate() {
        let mut expected = expected.clone();
        strip_scratch(&mut expected);
        let mut actual = actual.clone();
        pin_event_clock(&mut actual, &now);
        check(&format!("event {i}"), &expected, &actual);
    }
    let mut result = rust.result;
    if !result.is_null() {
        pin_clock(&mut result, &now);
    }
    check("result", &scenario["result"], &result);
    check("thrown", &scenario["thrown"], &rust.thrown);
    diffs
}

fn replay(names: &[&str]) {
    let scenarios = FIXTURE["scenarios"].as_array().unwrap();
    let mut failures = Vec::new();
    for name in names {
        let scenario = scenarios
            .iter()
            .find(|s| s["name"] == *name)
            .unwrap_or_else(|| panic!("no scenario {name}"));
        failures.extend(differences(scenario));
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

const ANTHROPIC_STREAM: &[&str] = &[
    "anthropic/thinking-text-tool-history",
    "anthropic/split-bytes-crlf",
    "anthropic/redacted-length-long-cache",
    "anthropic/repair-control-chars",
];
const ANTHROPIC_REQUESTS: &[&str] = &[
    "anthropic/env-long-retention",
    "anthropic/oauth-claude-code",
    "anthropic/managed-effort-mid-convo",
    "anthropic/beta-header-configured-and-suppressed",
    "anthropic/beta-null-suppresses",
    "anthropic/copilot-and-header-auth",
    "anthropic/openrouter-session-affinity-header-auth",
    "anthropic/on-payload",
    "anthropic/fallback-model-and-transformations",
];
const ANTHROPIC_ERRORS: &[&str] = &[
    "anthropic/http-400",
    "anthropic/sse-error-event",
    "anthropic/ended-before-stop",
    "anthropic/refusal",
    "anthropic/unknown-stop-reason",
    "anthropic/sensitive-and-pause",
    "anthropic/missing-key",
    "anthropic/missing-key-simple-throws",
];
const ANTHROPIC_RETRY: &[&str] = &[
    "anthropic/retry-429-then-ok",
    "anthropic/retry-delay-too-long",
    "anthropic/no-retry-should-retry-false",
];
const ANTHROPIC_ABORT: &[&str] = &["anthropic/abort-after-first-delta"];
const ANTHROPIC_SIMPLE: &[&str] = &[
    "anthropic/simple-reasoning-budget",
    "anthropic/simple-adaptive-catalog-model",
    "anthropic/simple-no-reasoning",
];
const COMPLETIONS_STREAM: &[&str] = &[
    "completions/reasoning-text-parallel-tools",
    "completions/openrouter-anthropic-cache-reasoning-details",
    "completions/grammar-custom-tool",
    "completions/moonshot-choice-usage",
    "completions/finish-inferred-tool-use",
];
const COMPLETIONS_REQUESTS: &[&str] = &[
    "completions/deepseek-detection",
    "completions/zai-tool-stream",
    "completions/requires-flags-and-tool-images",
    "completions/header-auth-and-on-payload",
    "completions/long-session-id-cache-key",
];
const COMPLETIONS_ERRORS: &[&str] = &[
    "completions/http-400-json",
    "completions/http-502-text",
    "completions/http-403-json-without-error",
    "completions/in-stream-error",
    "completions/content-filter",
    "completions/no-finish-reason",
    "completions/missing-key-throws-simple",
];
const COMPLETIONS_RETRY: &[&str] = &["completions/retry-500-then-ok"];
const COMPLETIONS_ABORT: &[&str] = &["completions/abort"];
const COMPLETIONS_SIMPLE: &[&str] = &[
    "completions/simple-chat-template-clamped",
    "completions/simple-off-qwen",
];
const RESPONSES_STREAM: &[&str] = &[
    "responses/reasoning-text-tool-history",
    "responses/grammar-custom-tool-and-priority-tier",
    "responses/refusal-and-final-answer-phase",
    "responses/backfill-encrypted-reasoning",
];
const RESPONSES_REQUESTS: &[&str] = &["responses/xai-copilot-no-max-output", "responses/simple-long-cache"];
const RESPONSES_ERRORS: &[&str] = &[
    "responses/incomplete-max-output",
    "responses/incomplete-content-filter",
    "responses/failed",
    "responses/error-event",
    "responses/no-terminal-event",
    "responses/http-401",
    "responses/http-401-other-provider",
];
const RESPONSES_ABORT: &[&str] = &["responses/abort"];

#[test]
fn ai_anthropic_streams_thinking_text_and_tool_calls() {
    replay(ANTHROPIC_STREAM);
}

#[test]
fn ai_anthropic_builds_requests_like_upstream() {
    replay(ANTHROPIC_REQUESTS);
}

#[test]
fn ai_anthropic_errors_and_stop_reasons() {
    replay(ANTHROPIC_ERRORS);
}

#[test]
fn ai_anthropic_retries_like_upstream() {
    replay(ANTHROPIC_RETRY);
}

#[test]
fn ai_anthropic_abort_mid_stream() {
    replay(ANTHROPIC_ABORT);
}

#[test]
fn ai_anthropic_simple_options() {
    replay(ANTHROPIC_SIMPLE);
}

#[test]
fn ai_completions_streams_reasoning_text_and_tool_calls() {
    replay(COMPLETIONS_STREAM);
}

#[test]
fn ai_completions_builds_requests_with_detected_compat() {
    replay(COMPLETIONS_REQUESTS);
}

#[test]
fn ai_completions_errors_and_stop_reasons() {
    replay(COMPLETIONS_ERRORS);
}

#[test]
fn ai_completions_retries_like_upstream() {
    replay(COMPLETIONS_RETRY);
}

#[test]
fn ai_completions_abort_mid_stream() {
    replay(COMPLETIONS_ABORT);
}

#[test]
fn ai_completions_simple_options() {
    replay(COMPLETIONS_SIMPLE);
}

#[test]
fn ai_responses_streams_reasoning_text_and_tool_calls() {
    replay(RESPONSES_STREAM);
}

#[test]
fn ai_responses_builds_requests_with_compat() {
    replay(RESPONSES_REQUESTS);
}

#[test]
fn ai_responses_errors_and_stop_reasons() {
    replay(RESPONSES_ERRORS);
}

#[test]
fn ai_responses_abort_mid_stream() {
    replay(RESPONSES_ABORT);
}

#[test]
fn ai_every_provider_fixture_scenario_is_replayed() {
    let groups = [
        ANTHROPIC_STREAM,
        ANTHROPIC_REQUESTS,
        ANTHROPIC_ERRORS,
        ANTHROPIC_RETRY,
        ANTHROPIC_ABORT,
        ANTHROPIC_SIMPLE,
        COMPLETIONS_STREAM,
        COMPLETIONS_REQUESTS,
        COMPLETIONS_ERRORS,
        COMPLETIONS_RETRY,
        COMPLETIONS_ABORT,
        COMPLETIONS_SIMPLE,
        RESPONSES_STREAM,
        RESPONSES_REQUESTS,
        RESPONSES_ERRORS,
        RESPONSES_ABORT,
    ];
    let covered: Vec<&str> = groups.iter().flat_map(|g| g.iter().copied()).collect();
    let missing: Vec<&str> = FIXTURE["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .filter(|n| !covered.contains(n))
        .collect();
    assert_eq!(missing, Vec::<&str>::new());
}
