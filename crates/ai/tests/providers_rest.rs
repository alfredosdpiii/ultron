//! Replays the fixtures recorded from the TypeScript providers by `fixtures/generate-rest.mjs` (azure-openai-responses,
//! openai-codex-responses, pi-messages, cloudflare): the same canned HTTP/SSE responses go through the Rust providers,
//! which must send the same requests (URL, method, headers minus SDK telemetry, JSON body including key order), emit
//! the same events, and return the same final message.

use ai::anthropic::{self, AnthropicOptions};
use ai::azure_openai_responses::AzureOpenAIResponsesOptions;
use ai::bedrock::{self, BedrockOptions};
use ai::cloudflare::resolve_cloudflare_model;
use ai::http::{BoxFuture, HeaderMap};
use ai::json::normalize_numbers;
use ai::openai_codex_responses::{self, OpenAICodexResponsesOptions};
use ai::openai_completions::{self, OpenAICompletionsOptions};
use ai::openai_responses::{self, OpenAIResponsesOptions};
use ai::options::pi_user_agent;
use ai::pi_messages::{self, PiMessagesOptions};
use ai::{
    AbortSignal, AssistantMessageEvent, Body, HttpRequest, HttpResponse, Model, SimpleStreamOptions, TranscriptContext,
    Transport,
};
use serde_json::{Map, Value, json};
use std::sync::{Arc, LazyLock, Mutex};

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).expect("fixture parses")
}

static AZURE: LazyLock<Value> = LazyLock::new(|| fixture(include_str!("../fixtures/azure.json")));
static CODEX: LazyLock<Value> = LazyLock::new(|| fixture(include_str!("../fixtures/codex.json")));
static PI_MESSAGES: LazyLock<Value> = LazyLock::new(|| fixture(include_str!("../fixtures/pi-messages.json")));
static BEDROCK: LazyLock<Value> = LazyLock::new(|| fixture(include_str!("../fixtures/bedrock.json")));
static CLOUDFLARE: LazyLock<Value> = LazyLock::new(|| fixture(include_str!("../fixtures/cloudflare.json")));

struct Canned {
    network_error: bool,
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
            if canned.network_error {
                return Err("fetch failed".to_string());
            }
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
    let chunks = response
        .get("chunks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|chunk| match chunk {
            Value::String(s) => s.as_bytes().to_vec(),
            Value::Array(bytes) => bytes.iter().map(|b| b.as_u64().unwrap() as u8).collect(),
            other => panic!("bad chunk {other}"),
        })
        .collect();
    Canned {
        network_error: response.get("networkError") == Some(&json!(true)),
        status: response.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
        headers,
        chunks,
    }
}

const SCRATCH_KEYS: &[&str] = &[
    "index",
    "partialJson",
    "partialArgs",
    "customInput",
    "streamIndex",
    "redactedChunks",
];

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
                if let Some(details) = diagnostic.get_mut("details").and_then(Value::as_object_mut)
                    && details.contains_key("timestampMs")
                {
                    details.insert("timestampMs".into(), now.clone());
                }
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
    let mut model: Model = serde_json::from_value(scenario["model"].clone()).expect("model");
    if scenario["wrapper"] == "cloudflare" {
        let env: Option<ai::env::ProviderEnv> =
            serde_json::from_value(scenario["options"]["env"].clone()).expect("env");
        model = resolve_cloudflare_model(&model, env.as_ref());
    }
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
    let fixtures_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
    let mut options: Value =
        serde_json::from_str(&scenario["options"].to_string().replace("<fixtures>", fixtures_dir)).unwrap();
    // The generator's process env disables instance metadata for the SDK chain; scope the same to these runs.
    // Every Bedrock run gets it, so no replay can reach a real instance metadata endpoint.
    if model.api == "bedrock-converse-stream" {
        if !options["env"].is_object() {
            options["env"] = json!({});
        }
        options["env"]["AWS_EC2_METADATA_DISABLED"] = json!("true");
    }
    let simple = scenario["fn"] == "streamSimple";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    let sink: &mut Sink<'_> = &mut sink;
    let outcome: Result<ai::AssistantMessage, String> =
        runtime.block_on(async { dispatch(&model, &context, &options, simple, &signal, &transport, sink).await });
    let user_agent = pi_user_agent();
    let requests = transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            let unrecorded: &[&str] = if model.api == "bedrock-converse-stream" {
                // The fixture leaves out what carries the SDK version, invocation ids or the wall clock.
                if scenario["recordAuthorization"] == true {
                    &["host", "x-amz-date", "x-amz-content-sha256"]
                } else {
                    &["authorization", "host", "x-amz-date", "x-amz-content-sha256"]
                }
            } else {
                &[]
            };
            let headers: Map<String, Value> = r
                .headers
                .iter()
                .filter(|(k, _)| !unrecorded.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), json!(v.replace(&user_agent, "<pi-user-agent>"))))
                .collect();
            // reqwest sends the WHATWG-normalized URL (`{` becomes `%7B`), as `fetch` does.
            let url = reqwest::Url::parse(&r.url).map_or(r.url.clone(), |u| u.to_string());
            json!({"url": url, "method": r.method, "headers": headers, "body": serde_json::from_str::<Value>(&r.body).unwrap()})
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
        let ordered = what.starts_with("request") && !(what.ends_with("body") && scenario["unorderedBody"] == true);
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
        if scenario["unorderedBody"] == true
            && let Some(event) = expected.as_object_mut()
        {
            // pi-messages spreads the wire event into the stream event; the Rust event type carries only its own fields.
            event.shift_remove("contentSignature");
            event.shift_remove("redacted");
            if event.get("type") == Some(&json!("toolcall_start")) {
                event.shift_remove("id");
                event.shift_remove("toolName");
            }
        }
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

macro_rules! parse_options {
    ($options:expr, $signal:expr, $ty:ty) => {{
        let mut parsed: $ty = serde_json::from_value($options.clone()).expect("options");
        parsed.base.signal = Some($signal.clone());
        parsed
    }};
}

async fn dispatch(
    model: &Model,
    context: &TranscriptContext,
    options: &Value,
    simple: bool,
    signal: &AbortSignal,
    transport: &FixtureTransport,
    sink: &mut Sink<'_>,
) -> Result<ai::AssistantMessage, String> {
    match (model.api.as_str(), simple) {
        // The azure and codex scenarios go through the legacy alias names (`streamAzureOpenAIResponses`, ...).
        ("azure-openai-responses", false) => Ok(ai::legacy::stream_azure_openai_responses(
            model,
            context,
            &parse_options!(options, signal, AzureOpenAIResponsesOptions),
            transport,
            sink,
        )
        .await),
        ("azure-openai-responses", true) => {
            ai::legacy::stream_simple_azure_openai_responses(
                model,
                context,
                &parse_options!(options, signal, SimpleStreamOptions),
                transport,
                sink,
            )
            .await
        }
        ("openai-codex-responses", false) => Ok(ai::legacy::stream_openai_codex_responses(
            model,
            context,
            &parse_options!(options, signal, OpenAICodexResponsesOptions),
            transport,
            sink,
        )
        .await),
        ("openai-codex-responses", true) => {
            ai::legacy::stream_simple_openai_codex_responses(
                model,
                context,
                &parse_options!(options, signal, SimpleStreamOptions),
                transport,
                sink,
            )
            .await
        }
        ("pi-messages", false) => Ok(pi_messages::stream(
            model,
            context,
            &parse_options!(options, signal, PiMessagesOptions),
            transport,
            sink,
        )
        .await),
        ("pi-messages", true) => Ok(pi_messages::stream_simple(
            model,
            context,
            &parse_options!(options, signal, SimpleStreamOptions),
            transport,
            sink,
        )
        .await),
        ("openai-completions", false) => Ok(openai_completions::stream(
            model,
            context,
            &parse_options!(options, signal, OpenAICompletionsOptions),
            transport,
            sink,
        )
        .await),
        ("openai-completions", true) => {
            openai_completions::stream_simple(
                model,
                context,
                &parse_options!(options, signal, SimpleStreamOptions),
                transport,
                sink,
            )
            .await
        }
        ("openai-responses", false) => Ok(openai_responses::stream(
            model,
            context,
            &parse_options!(options, signal, OpenAIResponsesOptions),
            transport,
            sink,
        )
        .await),
        ("anthropic-messages", false) => Ok(anthropic::stream(
            model,
            context,
            &parse_options!(options, signal, AnthropicOptions),
            transport,
            sink,
        )
        .await),
        ("bedrock-converse-stream", false) => Ok(bedrock::stream(
            model,
            context,
            &parse_options!(options, signal, BedrockOptions),
            transport,
            sink,
        )
        .await),
        ("bedrock-converse-stream", true) => Ok(bedrock::stream_simple(
            model,
            context,
            &parse_options!(options, signal, SimpleStreamOptions),
            transport,
            sink,
        )
        .await),
        (api, _) => panic!("unknown api {api}"),
    }
}

#[test]
fn ai_azure_replays_recorded_typescript_scenarios() {
    replay_all(&AZURE);
}

#[test]
fn ai_codex_replays_recorded_typescript_scenarios() {
    openai_codex_responses::reset_openai_codex_websocket_debug_stats(None);
    replay_all(&CODEX);
    // WebSocket fallback bookkeeping after the same scenarios, as `getOpenAICodexWebSocketDebugStats` reported it.
    for (session, expected) in CODEX["stats"].as_object().unwrap() {
        let actual = openai_codex_responses::get_openai_codex_websocket_debug_stats(session);
        assert_eq!(serde_json::to_value(actual).unwrap(), *expected, "stats for {session}");
    }
}

#[test]
fn ai_codex_usage_limit_message_counts_minutes_to_reset() {
    // Recorded TypeScript message for resets_at = now + 600 s: "Try again in ~10 min." (rounded, never negative).
    let now = 1_767_225_600_000;
    let body = json!({"error": {"code": "usage_limit_reached", "message": "limit", "plan_type": "PLUS", "resets_at": now / 1000 + 600}});
    let (message, friendly) = openai_codex_responses::parse_error_response(429, &body.to_string(), now);
    assert_eq!(message, "limit");
    assert_eq!(
        friendly.as_deref(),
        Some("You have hit your ChatGPT usage limit (plus plan). Try again in ~10 min.")
    );
    let half = json!({"error": {"type": "rate_limit_exceeded", "resets_at": now / 1000 + 90}});
    let (message, friendly) = openai_codex_responses::parse_error_response(400, &half.to_string(), now);
    assert_eq!(
        friendly.as_deref(),
        Some("You have hit your ChatGPT usage limit. Try again in ~2 min.")
    );
    assert_eq!(message, friendly.unwrap());
    assert_eq!(
        openai_codex_responses::parse_error_response(404, "", now),
        ("Not Found".to_string(), None)
    );
}

#[test]
fn ai_pi_messages_replays_recorded_typescript_scenarios() {
    replay_all(&PI_MESSAGES);
}

#[test]
fn ai_cloudflare_endpoints_replay_recorded_typescript_scenarios() {
    replay_all(&CLOUDFLARE);
}

#[test]
fn ai_bedrock_replays_recorded_typescript_scenarios() {
    replay_all(&BEDROCK);
}

#[test]
fn ai_bedrock_sigv4_signs_like_the_sdk_signer() {
    for case in BEDROCK["signing"].as_array().unwrap() {
        let request = &case["request"];
        let query = request["query"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let values = match v {
                    Value::Array(items) => items.iter().map(|i| i.as_str().unwrap().to_string()).collect(),
                    other => vec![other.as_str().unwrap().to_string()],
                };
                (k.clone(), values)
            })
            .collect();
        let mut signable = ai::aws::SignableRequest {
            method: request["method"].as_str().unwrap().into(),
            path: request["path"].as_str().unwrap().into(),
            query,
            headers: request["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect(),
            body: request["body"].as_str().unwrap().as_bytes().to_vec(),
        };
        let credentials = ai::aws::AwsCredentials {
            access_key_id: case["credentials"]["accessKeyId"].as_str().unwrap().into(),
            secret_access_key: case["credentials"]["secretAccessKey"].as_str().unwrap().into(),
            session_token: case["credentials"]["sessionToken"].as_str().map(str::to_string),
        };
        let date = case["date"].as_str().unwrap();
        let epoch_ms = match date {
            "2026-01-01T00:00:00Z" => 1_767_225_600_000,
            "2026-10-11T23:59:59Z" => 1_791_763_199_000,
            other => panic!("unknown date {other}"),
        };
        ai::aws::sign_request(
            &mut signable,
            &credentials,
            case["region"].as_str().unwrap(),
            "bedrock",
            epoch_ms,
        );
        let mut signed: Vec<(String, Value)> = signable
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), json!(v)))
            .collect();
        signed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut expected: Vec<(String, Value)> = case["signed"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(signed, expected, "signing {}", request["path"]);
    }
}

#[test]
fn ai_json_parse_errors_match_v8_messages() {
    for case in BEDROCK["jsonErrors"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        assert_eq!(
            json!(ai::jsutil::json_parse_error(input)),
            case["message"],
            "JSON.parse({input:?})"
        );
    }
}

static CLAUDE_CODE: LazyLock<Value> = LazyLock::new(|| fixture(include_str!("../fixtures/claude-code.json")));

/// Runs one Claude Code scenario against fixtures/fake-claude/claude with the recorded replies.
fn claude_differences(fixture: &Value, scenario: &Value) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let now = fixture["now"].clone();
    let fixtures_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
    let dir = tempfile::tempdir().unwrap();
    let dir_text = dir.path().to_string_lossy().to_string();
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
        "ULTRON_CLAUDE_CODE_BIN": format!("{fixtures_dir}/fake-claude/claude"),
        "ULTRON_CLAUDE_CODE_WARM": "0",
        "CLAUDECODE": "1",
        "CLAUDE_CODE_SESSION_ID": "parent",
        "FAKE_CLAUDE_REPLY": format!("{dir_text}/reply"),
        "FAKE_CLAUDE_LOG": log.to_string_lossy(),
    })
    .as_object()
    .unwrap()
    .clone();
    env.extend(scenario["env"].as_object().unwrap().clone());
    let mut options = scenario["options"].clone();
    options["env"] = Value::Object(env);
    let options: ai::claude_code_cli::ClaudeCodeOptions = serde_json::from_value(options).expect("options");
    let model: Model = serde_json::from_value(scenario["model"].clone()).unwrap();
    let context: TranscriptContext = serde_json::from_value(scenario["context"].clone()).unwrap();
    let mut events = Vec::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let result = runtime.block_on(ai::claude_code_cli::stream_simple(
        &model,
        &context,
        &options,
        &mut |event| {
            events.push(serde_json::to_value(event).unwrap());
        },
    ));
    let scrub = |value: Value| -> Value {
        serde_json::from_str(&value.to_string().replace(fixtures_dir, "<fixtures>")).unwrap()
    };
    let cli = std::fs::read_to_string(&log)
        .unwrap()
        .replace(&dir_text, "<dir>")
        .replace(fixtures_dir, "<fixtures>");
    let mut diffs = Vec::new();
    if json!(cli) != scenario["cli"] {
        diffs.push(format!(
            "{name}: cli log\n  expected {}\n  actual   {}",
            scenario["cli"],
            json!(cli)
        ));
    }
    let types = |list: &[Value]| list.iter().map(|e| e["type"].clone()).collect::<Vec<_>>();
    let expected_events = scenario["events"].as_array().unwrap();
    let actual_events: Vec<Value> = events.into_iter().map(scrub).collect();
    if types(expected_events) != types(&actual_events) {
        diffs.push(format!(
            "{name}: event types {:?} vs {:?}",
            types(expected_events),
            types(&actual_events)
        ));
    }
    for (i, (expected, actual)) in expected_events.iter().zip(&actual_events).enumerate() {
        let mut actual = actual.clone();
        pin_event_clock(&mut actual, &now);
        if normalize_numbers(expected.clone()) != normalize_numbers(actual.clone()) {
            diffs.push(format!("{name}: event {i}\n  expected {expected}\n  actual   {actual}"));
        }
    }
    let mut result = scrub(serde_json::to_value(result).unwrap());
    pin_clock(&mut result, &now);
    if normalize_numbers(scenario["result"].clone()) != normalize_numbers(result.clone()) {
        diffs.push(format!(
            "{name}: result\n  expected {}\n  actual   {result}",
            scenario["result"]
        ));
    }
    diffs
}

/// TypeScript's tools error (claude-code-cli.ts `TOOLS_UNSUPPORTED`). It recommended frames, which the Rust build
/// routes to the Claude subscription when they declare tools (`claude_subscription::claude_code_lane`), so this one
/// deliberate difference is replaced with the Rust text before comparing.
const TS_TOOLS_UNSUPPORTED: &str = "does not support tool calling here: `claude -p` returns completions, not raw tool calls, and only Ultron's session worker relays tools to it. Use it for tool-free lanes (rlm.infer/rlm.map frames, /review frames, judges), or run Claude Code as the root agent with `ultron --claude` or `ultron claude`.";

#[test]
fn ai_claude_code_cli_replays_recorded_typescript_scenarios() {
    ai::claude_code_cli::reset_claude_code_cli_state();
    let mut failures = Vec::new();
    let recorded = CLAUDE_CODE.to_string();
    assert!(
        recorded.contains(TS_TOOLS_UNSUPPORTED),
        "the fixture records TypeScript's tools error"
    );
    let fixture: Value =
        serde_json::from_str(&recorded.replace(TS_TOOLS_UNSUPPORTED, ai::claude_code_cli::TOOLS_UNSUPPORTED)).unwrap();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        failures.extend(claude_differences(&fixture, scenario));
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn ai_claude_code_cli_helpers_match_typescript() {
    let units = &CLAUDE_CODE["units"];
    for case in units["parseHelpFlags"].as_array().unwrap() {
        assert_eq!(
            json!(ai::claude_code_cli::parse_help_flags(case["help"].as_str().unwrap())),
            case["flags"]
        );
    }
    for case in units["usage"].as_array().unwrap() {
        let usage = ai::claude_code_cli::usage_from_result(&case["result"]);
        assert_eq!(
            normalize_numbers(serde_json::to_value(usage).unwrap()),
            normalize_numbers(case["usage"].clone())
        );
    }
    for case in units["reset"].as_array().unwrap() {
        let info = case["info"]
            .get("resetsAt")
            .map(|r| ai::claude_code_cli::RateLimitInfo {
                resets_at: r.as_f64(),
                ..Default::default()
            });
        assert_eq!(json!(ai::claude_code_cli::reset_text(info.as_ref())), case["text"]);
    }
}

// ------------------------------------------------------------------------------------------------- OAuth flows

static OAUTH: LazyLock<Value> = LazyLock::new(|| fixture(include_str!("../fixtures/oauth.json")));

thread_local! {
    static CLOCK_BASE: std::cell::Cell<Option<tokio::time::Instant>> = const { std::cell::Cell::new(None) };
}

/// `Date.now()` of the generator's fake clock: the recorded start plus the (paused) tokio time slept since.
fn oauth_clock() -> i64 {
    let base = CLOCK_BASE.with(|b| b.get()).expect("clock base");
    1_767_225_600_000 + (tokio::time::Instant::now() - base).as_millis() as i64
}

/// The generator's masking of random PKCE/state values.
fn mask_random(text: &str) -> String {
    let mut out = text.to_string();
    for prefix in ["code_verifier=", "code_challenge=", "state=", "\"state\":\""] {
        let mut result = String::new();
        let mut rest = out.as_str();
        while let Some(index) = rest.find(prefix) {
            result.push_str(&rest[..index + prefix.len()]);
            let after = &rest[index + prefix.len()..];
            let len = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '%' || c == '-'))
                .unwrap_or(after.len());
            if len > 0 {
                result.push_str("<random>");
            }
            rest = &after[len..];
        }
        result.push_str(rest);
        out = result;
    }
    // UUID v4 values.
    let chars: Vec<char> = out.chars().collect();
    let mut result = String::new();
    let mut i = 0;
    let is_uuid = |s: &[char]| {
        s.len() == 36
            && s.iter().enumerate().all(|(k, c)| match k {
                8 | 13 | 18 | 23 => *c == '-',
                14 => *c == '4',
                19 => matches!(c, '8' | '9' | 'a' | 'b'),
                _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
            })
    };
    while i < chars.len() {
        if i + 36 <= chars.len() && is_uuid(&chars[i..i + 36]) {
            result.push_str("<random>");
            i += 36;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

struct ScriptedInteraction {
    answers: Mutex<Vec<String>>,
    prompts: Mutex<Vec<Value>>,
    events: Mutex<Vec<Value>>,
    last_auth_url: Mutex<String>,
}

impl ai::auth::AuthInteraction for ScriptedInteraction {
    fn prompt<'a>(
        &'a self,
        prompt: ai::auth::AuthPrompt,
        _cancel: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.prompts
            .lock()
            .unwrap()
            .push(serde_json::to_value(&prompt).unwrap());
        let answer = {
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                None
            } else {
                Some(answers.remove(0))
            }
        };
        let url = self.last_auth_url.lock().unwrap().clone();
        let state = reqwest::Url::parse(if url.is_empty() { "http://x/" } else { &url })
            .ok()
            .and_then(|u| u.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.into_owned()))
            .unwrap_or_default();
        Box::pin(async move {
            answer
                .map(|a| a.replace("{STATE}", &state))
                .ok_or_else(|| "no answer".to_string())
        })
    }

    fn notify(&self, event: ai::auth::AuthEvent) {
        if let ai::auth::AuthEvent::AuthUrl { url, .. } = &event {
            *self.last_auth_url.lock().unwrap() = url.clone();
        }
        self.events.lock().unwrap().push(serde_json::to_value(&event).unwrap());
    }

    fn signal(&self) -> Option<AbortSignal> {
        Some(AbortSignal::new())
    }
}

fn oauth_flow(name: &str, transport: Arc<dyn Transport>) -> Box<dyn ai::auth::OAuthAuth> {
    match name {
        "xai" => Box::new(ai::oauth_xai::XaiOAuth {
            transport,
            clock: oauth_clock,
        }),
        "kimi" => Box::new(ai::oauth_kimi::KimiCodingOAuth {
            transport,
            clock: oauth_clock,
        }),
        "meta" => Box::new(ai::oauth_meta::MetaOAuth {
            transport,
            clock: oauth_clock,
        }),
        // A system-chosen callback port: the replay names it as the recording's registered 1456 (`registered_port`).
        "radius" => Box::new(
            ai::oauth_radius::RadiusOAuth::new("Radius", "gateway.example.com/", transport, oauth_clock)
                .with_callback_port(0),
        ),
        other => panic!("unknown flow {other}"),
    }
}

fn oauth_differences(scenario: &Value) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let transport = FixtureTransport::default();
    *transport.queue.lock().unwrap() = scenario["responses"].as_array().unwrap().iter().map(canned).collect();
    let shared: Arc<dyn Transport> = Arc::new(transport.clone());
    let flow = oauth_flow(scenario["flow"].as_str().unwrap(), shared);
    let interaction = ScriptedInteraction {
        answers: Mutex::new(serde_json::from_value(scenario["answers"].clone()).unwrap()),
        prompts: Mutex::default(),
        events: Mutex::default(),
        last_auth_url: Mutex::default(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let outcome = runtime.block_on(async {
        CLOCK_BASE.with(|b| b.set(Some(tokio::time::Instant::now())));
        if scenario["action"] == "login" {
            flow.login(&interaction).await
        } else {
            let credential = match serde_json::from_value(scenario["credential"].clone()).unwrap() {
                ai::auth::Credential::OAuth(c) => c,
                _ => panic!("oauth credential"),
            };
            flow.refresh(&credential, Some(AbortSignal::new())).await
        }
    });
    let (result, thrown, to_auth) = match outcome {
        Ok(credential) => (
            serde_json::to_value(ai::auth::Credential::OAuth(credential.clone())).unwrap(),
            Value::Null,
            serde_json::to_value(flow.to_auth(&credential).unwrap()).unwrap(),
        ),
        Err(message) => (Value::Null, json!(message), Value::Null),
    };
    let requests: Vec<Value> = transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            let body = if r.body.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&r.body).unwrap_or_else(|_| json!(r.body))
            };
            json!({"url": r.url, "method": r.method, "headers": r.headers, "body": body})
        })
        .collect();
    let events_text = json!(*interaction.events.lock().unwrap()).to_string();
    let port = registered_port(&events_text);
    let requests: Value = serde_json::from_str(&port(&mask_random(&json!(requests).to_string()))).unwrap();
    let events: Value = serde_json::from_str(&port(&mask_random(&events_text))).unwrap();
    let prompts: Value = serde_json::from_str(&port(&json!(*interaction.prompts.lock().unwrap()).to_string())).unwrap();
    let mut diffs = Vec::new();
    let mut check = |what: &str, expected: &Value, actual: &Value| {
        if normalize_numbers(expected.clone()) != normalize_numbers(actual.clone()) {
            diffs.push(format!("{name}: {what}\n  expected {expected}\n  actual   {actual}"));
        }
    };
    check("requests", &scenario["requests"], &requests);
    check("prompts", &scenario["prompts"], &prompts);
    check("events", &scenario["events"], &events);
    check("result", &scenario["result"], &result);
    check("thrown", &scenario["thrown"], &thrown);
    check("toAuth", &scenario["toAuth"], &to_auth);
    diffs
}

/// The Radius browser login listens on a port the system chose (`with_callback_port(0)`), named in its "Listening"
/// notice: the comparison names it as the recording's registered 1456, plain and URL-encoded.
fn registered_port(events: &str) -> impl Fn(&str) -> String {
    const PREFIX: &str = "Listening for OAuth callback on http://127.0.0.1:";
    let port = events
        .find(PREFIX)
        .map(|at| &events[at + PREFIX.len()..])
        .and_then(|rest| rest.split('/').next())
        .map(str::to_string);
    move |text: &str| match &port {
        Some(port) => text
            .replace(&format!("127.0.0.1:{port}/"), "127.0.0.1:1456/")
            .replace(&format!("127.0.0.1%3A{port}%2F"), "127.0.0.1%3A1456%2F"),
        None => text.to_string(),
    }
}

#[test]
fn ai_oauth_xai_kimi_meta_radius_replay_recorded_typescript_flows() {
    // Radius browser scenarios bind a system-chosen port, so no machine-wide port is held.
    let mut failures = Vec::new();
    for scenario in OAUTH["scenarios"].as_array().unwrap() {
        failures.extend(oauth_differences(scenario));
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn ai_oauth_pages_pkce_and_gateway_urls_match_typescript() {
    let units = &OAUTH["units"];
    assert_eq!(units["pkceChallengeMatchesVerifier"], true);
    let (verifier, challenge) = ai::oauth_flow::generate_pkce();
    assert_eq!(verifier.len(), 43);
    assert_eq!(ai::oauth_flow::pkce_challenge(&verifier), challenge);
    assert_eq!(
        json!(ai::oauth_flow::pkce_challenge(
            units["challenge"]["verifier"].as_str().unwrap()
        )),
        units["challenge"]["challenge"]
    );
    let pages = &units["pages"];
    assert_eq!(
        json!(ai::oauth_flow::oauth_success_html("Signed in to <Radius> & co.")),
        pages["success"]
    );
    assert_eq!(
        json!(ai::oauth_flow::oauth_error_html("Bad \"state\"", Some("it's <bad>"))),
        pages["error"]
    );
    assert_eq!(
        json!(ai::oauth_flow::oauth_error_html("Nope", None)),
        pages["errorNoDetails"]
    );
    for case in units["gateways"].as_array().unwrap() {
        assert_eq!(
            json!(ai::oauth_radius::normalize_radius_gateway_url(
                case["input"].as_str().unwrap()
            )),
            case["output"]
        );
    }
}
