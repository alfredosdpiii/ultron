//! Replays the fixtures recorded from the TypeScript providers by `fixtures/generate-google.mjs`: the same canned HTTP/SSE
//! responses go through the Rust providers, which must send the same requests (URL, method, headers minus SDK telemetry,
//! JSON body including key order), emit the same events, and return the same final message.

use ai::google::{self, GoogleOptions};
use ai::google_shared;
use ai::google_vertex::{self, GoogleVertexOptions};
use ai::http::{BoxFuture, HeaderMap};
use ai::json::normalize_numbers;
use ai::mistral::{self, MistralOptions};
use ai::options::pi_user_agent;
use ai::{
    AbortSignal, AssistantMessageEvent, Body, HttpRequest, HttpResponse, Model, SimpleStreamOptions, StopReason,
    TranscriptContext, Transport,
};
use serde_json::{Map, Value, json};
use std::sync::{Arc, LazyLock, Mutex};

static GOOGLE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/google.json")).expect("fixture parses"));
static VERTEX: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/google-vertex.json")).expect("fixture parses"));
static MISTRAL: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/mistral.json")).expect("fixture parses"));

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

type Sink<'a> = dyn FnMut(&AssistantMessageEvent) + Send + 'a;

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
    let sink: &mut Sink<'_> = &mut sink;
    let outcome: Result<ai::AssistantMessage, String> = runtime.block_on(async {
        macro_rules! with_signal {
            ($ty:ty) => {{
                let mut parsed: $ty = serde_json::from_value(options.clone()).expect("options");
                parsed.base.signal = Some(signal.clone());
                parsed
            }};
        }
        match (model.api.as_str(), simple) {
            ("google-generative-ai", false) => {
                Ok(google::stream(&model, &context, &with_signal!(GoogleOptions), &transport, sink).await)
            }
            ("google-generative-ai", true) => {
                google::stream_simple(&model, &context, &with_signal!(SimpleStreamOptions), &transport, sink).await
            }
            ("google-vertex", false) => {
                Ok(google_vertex::stream(&model, &context, &with_signal!(GoogleVertexOptions), &transport, sink).await)
            }
            ("google-vertex", true) => {
                google_vertex::stream_simple(&model, &context, &with_signal!(SimpleStreamOptions), &transport, sink)
                    .await
            }
            ("mistral-conversations", false) => {
                Ok(mistral::stream(&model, &context, &with_signal!(MistralOptions), &transport, sink).await)
            }
            ("mistral-conversations", true) => {
                mistral::stream_simple(&model, &context, &with_signal!(SimpleStreamOptions), &transport, sink).await
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
                .map(|(k, v)| (k.clone(), json!(v.replace(&user_agent, "<pi-user-agent>"))))
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

/// Generated tool call ids embed `Date.now()` (pinned upstream, the clock here): `name_<13 digits>_n` -> `name_<now>_n`.
fn pin_generated_ids(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_'
            && i + 14 < bytes.len()
            && bytes[i + 1..i + 14].iter().all(u8::is_ascii_digit)
            && bytes[i + 14] == b'_'
        {
            out.push_str("_<now>_");
            i += 15;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn compact(value: &Value) -> String {
    pin_generated_ids(&serde_json::to_string(&normalize_numbers(value.clone())).unwrap())
}

fn differences(fixture: &Value, scenario: &Value) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let now = fixture["now"].clone();
    let rust = run(scenario);
    let mut diffs = Vec::new();
    let mut check = |what: &str, expected: &Value, actual: &Value| {
        let ordered = what.starts_with("request");
        let (e, a) = (normalize_numbers(expected.clone()), normalize_numbers(actual.clone()));
        let (e, a) = if ordered {
            (json!(compact(&e)), json!(compact(&a)))
        } else {
            (
                serde_json::from_str(&compact(&e)).unwrap(),
                serde_json::from_str::<Value>(&compact(&a)).unwrap(),
            )
        };
        if e != a {
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

/// Replays every scenario of `fixture`, in recorded order (generated tool call ids count across scenarios).
fn replay_all(fixture: &Value) {
    let mut failures = Vec::new();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        failures.extend(differences(fixture, scenario));
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn ai_google_replays_recorded_typescript_scenarios() {
    replay_all(&GOOGLE);
}

#[test]
fn ai_google_vertex_replays_recorded_typescript_scenarios() {
    replay_all(&VERTEX);
}

#[test]
fn ai_mistral_replays_recorded_typescript_scenarios() {
    replay_all(&MISTRAL);
}

#[test]
fn ai_mistral_tool_call_ids_are_nine_alphanumerics() {
    // Already nine alphanumerics after stripping: kept as is.
    assert_eq!(mistral::derive_mistral_tool_call_id("abc-DEF_123", 0), "abcDEF123");
    // The recorded TypeScript request maps this foreign id to "88lrg5dqg" (fixtures/mistral.json).
    assert_eq!(
        mistral::derive_mistral_tool_call_id("call|weird+id/with=chars", 0),
        "88lrg5dqg"
    );
    let retry = mistral::derive_mistral_tool_call_id("abc-DEF_123", 1);
    assert_eq!(retry.len(), 9);
    assert_ne!(retry, "abcDEF123");
}

#[test]
fn ai_google_shared_model_predicates_match_typescript() {
    let units = &GOOGLE["units"];
    for case in units["usesThinkingLevel"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        assert_eq!(
            json!(google_shared::uses_google_thinking_level(id)),
            case["output"],
            "usesGoogleThinkingLevel({id})"
        );
    }
    for case in units["requiresToolCallId"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        assert_eq!(
            json!(google_shared::requires_tool_call_id(id)),
            case["output"],
            "requiresToolCallId({id})"
        );
    }
}

#[test]
fn ai_google_unknown_finish_reason_is_an_error() {
    assert_eq!(google_shared::map_stop_reason("SAFETY"), Ok(StopReason::Error));
    assert_eq!(
        google_shared::map_stop_reason("BRAND_NEW"),
        Err("Unhandled stop reason: BRAND_NEW".to_string())
    );
}
