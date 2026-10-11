//! Unit behaviors against values recorded from the TypeScript implementation (`fixtures/units.json`), serde shape
//! round-trips, the built-in model catalog, and the real HTTP transport against a local server.

use ai::constrained::make_strict_json_schema;
use ai::env::{EnvSource, env_api_key_in, find_env_keys_in};
use ai::json::{normalize_numbers, parse_json_with_repair, parse_streaming_json, repair_json, short_hash};
use ai::models::{
    builtin_model, builtin_models, builtin_providers, calculate_cost, clamp_thinking_level, supported_thinking_levels,
};
use ai::options::{ThinkingBudgets, adjust_max_tokens_for_thinking};
use ai::sse::SseDecoder;
use ai::transcript::{estimate_context_tokens, transform_messages};
use ai::{AssistantMessageEvent, Message, Model, StopReason, TranscriptContext, Usage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::LazyLock;

static UNITS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/units.json")).expect("units parse"));
static PROVIDERS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/providers.json")).expect("providers parse"));

fn cases(key: &str) -> &'static Vec<Value> {
    UNITS[key].as_array().unwrap()
}

#[test]
fn ai_parse_streaming_json_matches_upstream_for_every_prefix() {
    let mut failures = Vec::new();
    for case in cases("partialJson") {
        let input = case["input"].as_str().unwrap();
        let actual = normalize_numbers(parse_streaming_json(input));
        if actual != case["output"] {
            failures.push(format!("{input:?}: expected {} got {actual}", case["output"]));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn ai_repair_json_matches_upstream() {
    for case in cases("repairJson") {
        let input = case["input"].as_str().unwrap();
        assert_eq!(repair_json(input), case["output"].as_str().unwrap(), "{input:?}");
        let parsed = parse_json_with_repair(input).map_or(json!("<error>"), normalize_numbers);
        assert_eq!(parsed, case["parsed"], "{input:?}");
    }
}

#[test]
fn ai_short_hash_matches_upstream() {
    for case in cases("shortHash") {
        assert_eq!(
            short_hash(case["input"].as_str().unwrap()),
            case["output"].as_str().unwrap()
        );
    }
}

#[test]
fn ai_calculate_cost_uses_tiers_and_long_cache_writes() {
    let mut model: Model = serde_json::from_value(PROVIDERS["scenarios"][0]["model"].clone()).unwrap();
    model.cost = serde_json::from_value(UNITS["cost"]["model"]["cost"].clone()).unwrap();
    for case in UNITS["cost"]["cases"].as_array().unwrap() {
        let mut usage: Usage = serde_json::from_value(
            json!({
                "totalTokens": 0,
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
            })
            .as_object()
            .unwrap()
            .clone()
            .into_iter()
            .chain(case["usage"].as_object().unwrap().clone())
            .collect(),
        )
        .unwrap();
        calculate_cost(&model, &mut usage);
        assert_eq!(
            normalize_numbers(serde_json::to_value(&usage.cost).unwrap()),
            case["cost"],
            "{}",
            case["usage"]
        );
    }
}

#[test]
fn ai_thinking_levels_clamp_like_upstream() {
    let base = PROVIDERS["scenarios"][0]["model"].clone();
    for case in cases("thinking") {
        let mut value = base.clone();
        value["reasoning"] = case["model"]["reasoning"].clone();
        if let Some(map) = case["model"].get("thinkingLevelMap") {
            value["thinkingLevelMap"] = map.clone();
        }
        let model: Model = serde_json::from_value(value).unwrap();
        assert_eq!(
            json!(supported_thinking_levels(&model)),
            case["supported"],
            "{}",
            case["model"]
        );
        for (level, expected) in case["clamped"].as_object().unwrap() {
            assert_eq!(
                clamp_thinking_level(&model, level),
                expected.as_str().unwrap(),
                "{} {level}",
                case["model"]
            );
        }
    }
}

#[test]
fn ai_env_api_keys_resolve_like_upstream() {
    for case in cases("env") {
        let env: HashMap<String, String> = serde_json::from_value(case["env"].clone()).unwrap();
        let lookup = |name: &str| env.get(name).filter(|v| !v.is_empty()).cloned();
        let exists = |_: &std::path::Path| false;
        let source = EnvSource {
            lookup: &lookup,
            exists: &exists,
            home: None,
        };
        let provider = case["provider"].as_str().unwrap();
        assert_eq!(
            json!(find_env_keys_in(provider, &source)),
            case["found"],
            "{provider} {env:?}"
        );
        assert_eq!(
            json!(env_api_key_in(provider, &source)),
            case["key"],
            "{provider} {env:?}"
        );
    }
}

#[test]
fn ai_env_api_key_reports_vertex_adc_when_credentials_exist() {
    let env: HashMap<&str, &str> = [("GOOGLE_CLOUD_PROJECT", "p"), ("GOOGLE_CLOUD_LOCATION", "l")]
        .into_iter()
        .collect();
    let lookup = |name: &str| env.get(name).map(|v| v.to_string());
    let exists = |path: &std::path::Path| {
        path == std::path::Path::new("/home/u/.config/gcloud/application_default_credentials.json")
    };
    let source = EnvSource {
        lookup: &lookup,
        exists: &exists,
        home: Some("/home/u".into()),
    };
    assert_eq!(
        env_api_key_in("google-vertex", &source).as_deref(),
        Some("<authenticated>")
    );
    let missing = EnvSource {
        lookup: &lookup,
        exists: &exists,
        home: Some("/elsewhere".into()),
    };
    assert_eq!(env_api_key_in("google-vertex", &missing), None);
}

#[test]
fn ai_strict_json_schema_matches_upstream() {
    for case in cases("strictSchemas") {
        match make_strict_json_schema(&case["schema"]) {
            Ok(strict) => assert_eq!(
                serde_json::to_string(&strict).unwrap(),
                serde_json::to_string(&case["strict"]).unwrap(),
                "{}",
                case["schema"]
            ),
            Err(message) => assert_eq!(json!(message), case["error"], "{}", case["schema"]),
        }
    }
}

#[test]
fn ai_estimate_context_tokens_matches_upstream() {
    for case in cases("estimate") {
        let context: TranscriptContext = serde_json::from_value(case["context"].clone()).unwrap();
        assert_eq!(
            serde_json::to_value(estimate_context_tokens(&context.messages)).unwrap(),
            case["estimate"]
        );
    }
}

#[test]
fn ai_adjust_max_tokens_for_thinking_matches_upstream() {
    for case in cases("maxTokens") {
        let budgets: Option<ThinkingBudgets> = serde_json::from_value(case["budgets"].clone()).unwrap();
        let (max_tokens, budget) = adjust_max_tokens_for_thinking(
            case["base"].as_u64(),
            case["cap"].as_u64().unwrap(),
            case["level"].as_str().unwrap(),
            budgets.as_ref(),
        );
        assert_eq!(
            json!({"maxTokens": max_tokens, "thinkingBudget": budget}),
            case["result"],
            "{case}"
        );
    }
}

#[test]
fn ai_transform_messages_matches_upstream() {
    let transform = &UNITS["transform"];
    let model: Model = serde_json::from_value(transform["model"].clone()).unwrap();
    let input: Vec<Message> = serde_json::from_value(transform["input"].clone()).unwrap();
    let normalize = |id: &str, _: &ai::AssistantMessage| {
        id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect::<String>()
    };
    let now = PROVIDERS["now"].as_i64().unwrap();
    let output = transform_messages(&input, &model, Some(&normalize), now);
    assert_eq!(
        normalize_numbers(serde_json::to_value(output).unwrap()),
        transform["output"]
    );
}

/// Every recorded message and event deserializes into the Rust types and serializes back to the same JSON.
#[test]
fn ai_message_and_event_serde_shapes_match_typescript_json() {
    let mut checked = 0;
    for scenario in PROVIDERS["scenarios"].as_array().unwrap() {
        for message in scenario["context"]["messages"].as_array().unwrap() {
            let parsed: Message = serde_json::from_value(message.clone()).unwrap_or_else(|e| panic!("{e}: {message}"));
            assert_eq!(normalize_numbers(serde_json::to_value(&parsed).unwrap()), *message);
            checked += 1;
        }
        for event in scenario["events"].as_array().unwrap() {
            let mut event = event.clone();
            for key in ["partial", "message", "error"] {
                if let Some(Value::Array(content)) = event.get_mut(key).and_then(|m| m.get_mut("content")) {
                    for block in content {
                        for scratch in ["index", "partialJson", "partialArgs", "customInput", "streamIndex"] {
                            block.as_object_mut().unwrap().shift_remove(scratch);
                        }
                    }
                }
            }
            let parsed: AssistantMessageEvent =
                serde_json::from_value(event.clone()).unwrap_or_else(|e| panic!("{e}: {event}"));
            assert_eq!(
                normalize_numbers(serde_json::to_value(&parsed).unwrap()),
                normalize_numbers(event)
            );
            checked += 1;
        }
    }
    assert!(checked > 400, "only {checked} values checked");
    let tagged: Value = json!({"role": "user", "content": "x", "timestamp": 1});
    assert!(matches!(
        serde_json::from_value::<Message>(tagged).unwrap(),
        Message::User(_)
    ));
    assert!(serde_json::from_value::<Message>(json!({"role": "bogus", "content": "x", "timestamp": 1})).is_err());
    assert_eq!(serde_json::to_value(StopReason::ToolUse).unwrap(), json!("toolUse"));
}

#[test]
fn ai_builtin_catalog_round_trips_every_model() {
    let raw: Value = serde_json::from_str(include_str!("../data/models.json")).unwrap();
    let providers: Vec<&str> = builtin_providers().collect();
    assert_eq!(
        providers,
        raw.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>()
    );
    let mut count = 0;
    for (provider, models) in raw.as_object().unwrap() {
        let ids: Vec<&str> = builtin_models(provider).map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            models
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        for (id, value) in models.as_object().unwrap() {
            let model = builtin_model(provider, id).unwrap();
            assert_eq!(
                &normalize_numbers(serde_json::to_value(model).unwrap()),
                value,
                "{provider}/{id}"
            );
            count += 1;
        }
    }
    assert!(count > 500, "{count} models");
    assert!(builtin_model("anthropic", "no-such-model").is_none());
    assert_eq!(builtin_models("no-such-provider").count(), 0);
}

/// Divergence: upstream's Anthropic decoder reads a CR ending one chunk and the LF starting the next as two line
/// breaks, which ends the event early; the Rust decoder keeps CRLF one line break across chunks.
#[test]
fn ai_sse_crlf_split_across_chunks_is_one_line_break() {
    let mut decoder = SseDecoder::default();
    assert!(decoder.push(b"event: ping\r").is_empty());
    assert!(decoder.push(b"\ndata: {}\r\n").is_empty());
    let events = decoder.push(b"\r\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("ping"));
    assert_eq!(events[0].data, "{}");
    let mut trailing = SseDecoder::default();
    assert!(trailing.push(b"data: a\ndata: b").is_empty());
    assert_eq!(trailing.finish(true)[0].data, "a\nb");
    let mut dropped = SseDecoder::default();
    dropped.push(b"data: a\n");
    assert!(dropped.finish(false).is_empty());
}

/// The reqwest transport streams a real HTTP response from a local server through the Anthropic provider.
#[test]
fn ai_reqwest_transport_streams_from_a_local_server() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let body = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"test-model\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"over http\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let n = socket.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..n]);
            let text = String::from_utf8_lossy(&request);
            if let Some(head_end) = text.find("\r\n\r\n") {
                let length = text[..head_end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + length {
                    break;
                }
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).unwrap();
        String::from_utf8(request).unwrap()
    });
    let mut model: Model = serde_json::from_value(PROVIDERS["scenarios"][0]["model"].clone()).unwrap();
    model.base_url = format!("http://{address}");
    let context = TranscriptContext {
        messages: serde_json::from_value(json!([{"role": "user", "content": "hi", "timestamp": 1}])).unwrap(),
    };
    let options = ai::anthropic::AnthropicOptions {
        base: ai::StreamOptions {
            api_key: Some("sk-local".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let transport = ai::ReqwestTransport::with_client(reqwest::Client::builder().no_proxy().build().unwrap(), None);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut types = Vec::new();
    let message = runtime.block_on(ai::anthropic::stream(
        &model,
        &context,
        &options,
        &transport,
        &mut |e: &AssistantMessageEvent| {
            types.push(serde_json::to_value(e).unwrap()["type"].clone());
        },
    ));
    let request = server.join().unwrap();
    assert!(
        request.starts_with("POST /v1/messages?beta=true HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.to_ascii_lowercase().contains("\r\nx-api-key: sk-local\r\n"),
        "{request}"
    );
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(
        serde_json::to_value(&message.content).unwrap(),
        json!([{"type": "text", "text": "over http"}])
    );
    assert_eq!(
        json!(types),
        json!(["start", "text_start", "text_delta", "text_end", "done"])
    );
}
