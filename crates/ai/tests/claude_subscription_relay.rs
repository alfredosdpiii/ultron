//! The whole path, fake `claude` -> admission relay -> fake upstream: ports of the Hermes plugin's
//! `tests/test_directsdk_admission.py` (one upstream admission and first-response authority over native recovery)
//! and `test_admission_restoration.py::test_actual_client_replays_restored_parallel_rounds_after_recreation`.

mod claude_subscription_support;

use ai::claude_subscription::history::CARRIER_KEY;
use ai::types::{AssistantContent, Message, StopReason};
use ai::{AbortSignal, AssistantMessage, SimpleStreamOptions};
use claude_subscription_support::*;
use serde_json::{Value, json};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn zero() -> Value {
    json!({"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0})
}

fn turn(tools: bool) -> ai::TranscriptContext {
    let tools = if tools { vec![rlm_tool()] } else { vec![] };
    context(vec![system("fixture system", tools), user("fixture")])
}

fn options() -> SimpleStreamOptions {
    SimpleStreamOptions::default()
}

fn native_admission(message: &AssistantMessage) -> Value {
    diagnostic(message, "claude_subscription_usage")
        .unwrap()
        .details
        .as_ref()
        .unwrap()["native_admission"]
        .clone()
}

#[test]
fn ai_claude_subscription_first_response_owns_usage_and_stops_recovery() {
    seal();
    for (stop, expected) in [
        ("end_turn", StopReason::Stop),
        ("max_tokens", StopReason::Length),
        ("model_context_window_exceeded", StopReason::Length),
    ] {
        let fake = Fake::new();
        let upstream = Upstream::start(move |_, stream| {
            reply_sse(
                stream,
                &reply_events("first", &[json!({"type": "text", "text": "FIRST"})], stop, zero()),
            )
        });
        let (message, _) = call(
            fake.config(&upstream.url(), &[("FAKE_RETRIES", "1")]),
            &model("sonnet"),
            &turn(false),
            &options(),
        );
        assert_eq!(upstream.requests().len(), 1);
        assert_eq!(message.stop_reason, expected, "{stop}: {:?}", message.error_message);
        assert_eq!(
            message.content,
            vec![AssistantContent::Text(ai::TextContent {
                text: "FIRST".into(),
                text_signature: None
            })]
        );
        assert_eq!(message.usage.input, 0);
        let carrier = &message.provider_metadata.as_ref().unwrap()[CARRIER_KEY];
        assert_eq!(carrier["messages"][0]["stop_reason"], stop);
        assert_eq!(
            native_admission(&message),
            json!({"upstream_requests": 1, "blocked_requests": 1, "request_id": "req_fixture"})
        );
        // Native's recovery attempt was answered locally.
        let attempts = &fake.last()["attempts"];
        assert_eq!(attempts[1]["status"], 400);
        assert!(
            attempts[1]["body"]
                .as_str()
                .unwrap()
                .contains("ULTRON_MODEL_ADMISSION_CONSUMED")
        );
    }
}

fn refusal_events(details: Option<Value>, tool: bool) -> Vec<Value> {
    let mut events = vec![
        json!({"type": "message_start", "message": {"id": "first", "role": "assistant", "model": "sonnet", "content": [], "usage": zero()}}),
    ];
    if tool {
        events.extend([
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "mcp__ultron__rlm", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": ""}}),
            json!({"type": "content_block_stop", "index": 0}),
        ]);
    }
    let mut delta = json!({"stop_reason": "refusal"});
    if let Some(details) = details {
        delta["stop_details"] = details;
    }
    events.push(json!({"type": "message_delta", "delta": delta, "usage": zero()}));
    events.push(json!({"type": "message_stop"}));
    events
}

#[test]
fn ai_claude_subscription_a_refusal_is_terminal_and_never_runs_a_cut_off_call() {
    seal();
    let cases = [
        (
            Some(json!({"category": "cyber", "explanation": "The request was declined."})),
            "The request was declined.",
        ),
        (
            Some(json!({"category": "cyber", "explanation": null})),
            "provider refusal category: cyber",
        ),
        (None, "The model refused to complete the request"),
    ];
    for (details, refusal) in cases {
        for tool in [false, true] {
            let fake = Fake::new();
            let events = refusal_events(details.clone(), tool);
            let upstream = Upstream::start(move |_, stream| reply_sse(stream, &events));
            let (message, events) = call(
                fake.config(&upstream.url(), &[]),
                &model("sonnet"),
                &turn(tool),
                &options(),
            );
            assert_eq!(message.stop_reason, StopReason::Error);
            assert_eq!(message.error_message.as_deref(), Some(refusal));
            assert_eq!(message.raw_stop_reason.as_deref(), Some("refusal"));
            assert!(
                message.content.is_empty(),
                "nothing for Ultron to run: {:?}",
                message.content
            );
            assert!(matches!(events.last(), Some(ai::AssistantMessageEvent::Error { .. })));
            // The native turn is kept whole in the carrier, refused tool_use included.
            let native = &message.provider_metadata.as_ref().unwrap()[CARRIER_KEY]["messages"][0];
            assert_eq!(native["stop_reason"], "refusal");
            let kinds: Vec<&str> = native["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["type"].as_str().unwrap())
                .collect();
            assert_eq!(kinds, if tool { vec!["tool_use"] } else { vec![] });
        }
    }
}

#[test]
fn ai_claude_subscription_empty_tool_input_completes_the_capture() {
    seal();
    let fake = Fake::new();
    let events = vec![
        json!({"type": "message_start", "message": {"id": "first", "role": "assistant", "model": "sonnet", "content": [], "usage": zero()}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "mcp__ultron__rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": ""}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": zero()}),
        json!({"type": "message_stop"}),
    ];
    let upstream = Upstream::start(move |_, stream| reply_sse(stream, &events));
    let (message, _) = call(
        fake.config(&upstream.url(), &[]),
        &model("sonnet"),
        &turn(true),
        &options(),
    );
    assert_eq!(upstream.requests().len(), 1);
    assert_eq!(message.stop_reason, StopReason::ToolUse, "{:?}", message.error_message);
    let AssistantContent::ToolCall(tool) = &message.content[0] else {
        panic!("{:?}", message.content)
    };
    assert_eq!((tool.name.as_str(), &tool.arguments), ("rlm", &json!({})));
}

#[test]
fn ai_claude_subscription_cancel_closes_the_active_upstream_socket() {
    seal();
    let fake = Fake::new();
    let entered = Arc::new(AtomicBool::new(false));
    let disconnected = Arc::new(AtomicBool::new(false));
    let upstream = {
        let (entered, disconnected) = (entered.clone(), disconnected.clone());
        Upstream::start(move |_, stream| {
            entered.store(true, Ordering::SeqCst);
            // Far longer than the test: only the relay closing the socket ends this read.
            stream.set_read_timeout(Some(Duration::from_secs(600))).unwrap();
            let mut byte = [0u8; 1];
            if matches!(stream.read(&mut byte), Ok(0) | Err(_)) {
                disconnected.store(true, Ordering::SeqCst);
            }
        })
    };
    let signal = AbortSignal::new();
    let mut opts = options();
    opts.base.signal = Some(signal.clone());
    let aborter = {
        let (signal, entered) = (signal.clone(), entered.clone());
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(120);
            while !entered.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            signal.abort();
        })
    };
    let (message, _) = call(fake.config(&upstream.url(), &[]), &model("sonnet"), &turn(false), &opts);
    aborter.join().unwrap();
    assert!(entered.load(Ordering::SeqCst));
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.error_message.as_deref(), Some("Claude request cancelled"));
    let deadline = Instant::now() + Duration::from_secs(120);
    while !disconnected.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(disconnected.load(Ordering::SeqCst), "the upstream socket was closed");
}

#[test]
fn ai_claude_subscription_incomplete_upstream_error_names_the_first_attempt() {
    seal();
    let fake = Fake::new();
    let upstream = Upstream::start(|_, stream| {
        reply_status(
            stream,
            529,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213000 tokens > 200000 maximum"}}"#,
        )
    });
    let (message, _) = call(
        fake.config(&upstream.url(), &[]),
        &model("sonnet"),
        &turn(false),
        &options(),
    );
    let error = message.error_message.unwrap();
    assert!(error.starts_with("Incomplete upstream response (first upstream attempt: status 529, capture incomplete, native retries denied: 0, upstream said: prompt is too long: 213000 tokens"), "{error}");
}

#[test]
fn ai_claude_subscription_incomplete_upstream_error_carries_the_first_status() {
    seal();
    for status in [429u16, 401, 400, 500, 529, 200] {
        let fake = Fake::new();
        let upstream = Upstream::start(move |_, stream| {
            if status == 200 {
                // Headers and message_start, then the connection drops: no complete capture.
                let body = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"cut\",\"role\":\"assistant\",\"content\":[]}}\n\n";
                let _ = std::io::Write::write_all(
                    stream,
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                        .as_bytes(),
                );
                return;
            }
            reply_status(
                stream,
                status,
                r#"{"type":"error","error":{"type":"api_error","message":"fixture"}}"#,
            );
        });
        let (message, _) = call(
            fake.config(&upstream.url(), &[]),
            &model("sonnet"),
            &turn(false),
            &options(),
        );
        let error = message.error_message.clone().unwrap();
        assert!(
            error.starts_with(&format!(
                "Incomplete upstream response (first upstream attempt: status {status}, capture incomplete"
            )),
            "{error}"
        );
        let details = diagnostic(&message, "claude_subscription_error")
            .unwrap()
            .details
            .clone()
            .unwrap();
        assert_eq!(
            details["status"],
            if status == 200 { Value::Null } else { json!(status) }
        );
    }
}

#[test]
fn ai_claude_subscription_silent_upstream_fails_fast_and_pings_keep_a_slow_one_alive() {
    seal();
    let fake = Fake::new();
    let release = Arc::new(AtomicBool::new(false));
    let silent = {
        let release = release.clone();
        Upstream::start(move |_, stream| {
            let start = json!({"type": "message_start", "message": {"id": "m", "role": "assistant", "model": "sonnet", "content": [], "usage": {"input_tokens": 0, "output_tokens": 0}}});
            let _ = std::io::Write::write_all(
                stream,
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {start}\n\n").as_bytes(),
            );
            let deadline = Instant::now() + Duration::from_secs(600);
            while !release.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let mut config = fake.config(&silent.url(), &[]);
    config.idle = Duration::from_secs(1);
    config.timeout = Duration::from_secs(600);
    let began = Instant::now();
    let (message, _) = call(config, &model("sonnet"), &turn(false), &options());
    release.store(true, Ordering::SeqCst);
    let error = message.error_message.unwrap();
    assert!(
        error.contains("status 200, capture incomplete, relay failure UpstreamIdle"),
        "{error}"
    );
    // The 1 s idle budget ended it, not the overall timeout (a hang guard, loose for load).
    assert!(began.elapsed() < Duration::from_secs(120));

    // Pings every 0.5 s for 3 s keep a 2 s idle budget from firing. Under load the upstream thread can stall past
    // the budget; the idle failure is then right, and the call is retried until the upstream kept its pace.
    for _attempt in 0..20 {
        let slow = Upstream::start(|_, stream| {
            let usage = json!({"input_tokens": 0, "output_tokens": 0});
            let _ = std::io::Write::write_all(stream, b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
            let send = |stream: &mut std::net::TcpStream, event: Value| {
                let _ = std::io::Write::write_all(stream, format!("data: {event}\n\n").as_bytes());
            };
            let events = reply_events("first", &[json!({"type": "text", "text": "FIRST"})], "end_turn", usage);
            send(stream, events[0].clone());
            for _ in 0..6 {
                std::thread::sleep(Duration::from_millis(500));
                send(stream, json!({"type": "ping"}));
            }
            for event in &events[1..] {
                send(stream, event.clone());
            }
        });
        let mut config = fake.config(&slow.url(), &[]);
        config.idle = Duration::from_secs(2);
        let (message, _) = call(config, &model("sonnet"), &turn(false), &options());
        if message
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("relay failure UpstreamIdle"))
        {
            continue;
        }
        assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
        return;
    }
    panic!("the upstream never kept a 0.5 s ping pace in 20 attempts");
}

#[test]
fn ai_claude_subscription_an_old_cli_error_names_the_minimum_version() {
    seal();
    let fake = Fake::new();
    let upstream = Upstream::start(|_, stream| {
        reply_status(
            stream,
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Claude Code 2.1.200 does not support this model"}}"#,
        )
    });
    let (message, _) = call(
        fake.config(&upstream.url(), &[]),
        &model("sonnet"),
        &turn(false),
        &options(),
    );
    let error = message.error_message.unwrap();
    assert!(
        error.ends_with("(Sonnet 5.5 needs Claude Code 2.1.284 or newer; run `claude update`)"),
        "{error}"
    );
}

#[test]
fn ai_claude_subscription_first_turn_frame_goes_ahead_of_native_context() {
    seal();
    let fake = Fake::new();
    let upstream = Upstream::start(|_, stream| {
        reply_sse(
            stream,
            &reply_events("m", &[json!({"type": "text", "text": "ok"})], "end_turn", zero()),
        )
    });
    let (message, _) = call(
        fake.config(&upstream.url(), &[("FAKE_PREPEND", "1"), ("FAKE_MARK", "1")]),
        &model("sonnet"),
        &turn(false),
        &options(),
    );
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let body = upstream.requests()[0].json();
    let first = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(first[0]["text"], "fixture");
    assert_eq!(first[0]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
    assert!(first[1]["text"].as_str().unwrap().starts_with("<system-reminder>"));
    assert!(first[1].get("cache_control").is_none());
}

#[test]
fn ai_claude_subscription_actual_client_replays_restored_parallel_rounds_across_calls() {
    seal();
    let fake = Fake::new();
    let turn_count = Arc::new(Mutex::new(0usize));
    let upstream = {
        let turn_count = turn_count.clone();
        Upstream::start(move |_, stream| {
            let n = {
                let mut count = turn_count.lock().unwrap();
                *count += 1;
                *count
            };
            let blocks = [
                json!({"type": "thinking", "thinking": "fixture reasoning", "signature": format!("signed-{n}")}),
                json!({"type": "text", "text": "Calling parallel cells."}),
                json!({"type": "tool_use", "id": format!("r{n}a"), "name": "mcp__ultron__rlm", "input": {"code": "a"}}),
                json!({"type": "tool_use", "id": format!("r{n}b"), "name": "mcp__ultron__rlm", "input": {"code": "b"}}),
            ];
            reply_sse(
                stream,
                &reply_events(
                    &format!("m{n}"),
                    &blocks,
                    "tool_use",
                    json!({"input_tokens": 0, "output_tokens": 0}),
                ),
            );
        })
    };
    let mut messages = turn(true).messages;
    let vars = [("FAKE_REMINDER", "1"), ("FAKE_MARK", "1")];
    for round in 0..4 {
        let (result, _) = call(
            fake.config(&upstream.url(), &vars),
            &model("sonnet"),
            &context(messages.clone()),
            &options(),
        );
        assert_eq!(result.stop_reason, StopReason::ToolUse, "{:?}", result.error_message);
        let calls: Vec<String> = result
            .content
            .iter()
            .filter_map(|b| match b {
                AssistantContent::ToolCall(c) => Some(c.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        let seen = upstream.requests();
        let current = seen.last().unwrap().json()["messages"].clone();
        let frames = fake.last()["frames"].clone();
        // What went upstream is exactly what Ultron replayed (native's reminder removed, markers aside).
        let strip = |messages: &Value| -> Value {
            let mut messages = messages.clone();
            for m in messages.as_array_mut().unwrap() {
                for b in m["content"].as_array_mut().unwrap() {
                    b.as_object_mut().unwrap().remove("cache_control");
                }
            }
            messages
        };
        let expected: Vec<Value> = frames
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["message"].clone())
            .collect();
        if round == 0 {
            // A plain user frame is outside restoration's scope: native's addition goes up as native sent it.
            assert_eq!(strip(&current)[0]["content"][0], expected[0]["content"][0]);
            assert!(
                current[0]["content"][0].get("cache_control").is_some(),
                "the breakpoint sits on Ultron's frame"
            );
            assert!(
                current[0]["content"][1]["text"]
                    .as_str()
                    .unwrap()
                    .contains("<system-reminder>userEmail")
            );
        } else {
            assert_eq!(strip(&current), Value::Array(expected));
        }
        if round > 0 {
            assert_eq!(
                current.as_array().unwrap().last().unwrap()["content"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["cache_control"],
                json!({"type": "ephemeral", "ttl": "1h"})
            );
            if round > 1 {
                let previous = strip(&seen[seen.len() - 2].json()["messages"]);
                let n = previous.as_array().unwrap().len();
                assert_eq!(
                    strip(&current).as_array().unwrap()[..n],
                    previous.as_array().unwrap()[..]
                );
            }
            assert!(
                diagnostic(&result, "claude_subscription_usage")
                    .unwrap()
                    .details
                    .as_ref()
                    .unwrap()["native_admission"]
                    .get("unrestored")
                    .is_none()
            );
        }
        messages.push(Message::Assistant(result));
        for id in calls {
            messages.push(tool_result(
                &id,
                &format!("host <system-reminder>literal</system-reminder> {id}"),
            ));
        }
    }
    assert_eq!(upstream.requests().len(), 4);
    // The signed thinking of every earlier round replayed verbatim.
    let last = upstream.requests().last().unwrap().json();
    let signatures: Vec<&str> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .map(|m| m["content"][0]["signature"].as_str().unwrap())
        .collect();
    assert_eq!(signatures, ["signed-1", "signed-2", "signed-3"]);
}
