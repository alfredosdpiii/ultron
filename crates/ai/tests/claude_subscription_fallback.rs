//! Subscription first, API credits second (`claude_subscription::fallback`): a fake `claude`
//! (`fixtures/claude-subscription/claude.py`) on a fake subscription upstream, and a fake Anthropic Messages API on
//! loopback. Every router gets a private agent directory, an injected environment and an injected clock: no test
//! reads the real environment, ~/.ultron or ~/.claude, and none reaches the network.

mod claude_subscription_support;

use ai::claude_subscription::fallback::{
    self, BOTH_EXHAUSTED, DEFAULT_HOLD_MS, KEY_HINT, ROUTE_DIAGNOSTIC, Router, RouterOptions, SUBSCRIPTION_NOTICE,
};
use ai::claude_subscription::limit::{Signals, usage_limit};
use ai::claude_subscription::{ClaudeError, ERROR_DIAGNOSTIC};
use ai::types::{AssistantContent, AssistantMessageEvent, Message, TranscriptContext};
use ai::{AssistantMessage, Model, ReqwestTransport, SimpleStreamOptions, StopReason};
use claude_subscription_support::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const NO_UPSTREAM: &str = "http://127.0.0.1:9";
const KEY: &str = "sk-ant-api03-fixture-credits-key";
/// The recorded native usage-limit text of the Hermes plugin tests (no reset date in it).
const HIT_LIMIT: &str = "rate_limit:You've hit your session limit · resets 3:45pm";
/// The recorded TypeScript fake's reset (`fake-claude.mjs`, `rate_limit` scenario).
const RESETS_AT: i64 = 1_790_700_600;
const NOW: i64 = 1_790_000_000_000;

fn api_sse(stream: &mut TcpStream, text: &str) {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_api", "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 3, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ];
    let body: String = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

const CREDITS_GONE: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#;

/// The fake API: streams "from api", or answers the credits-exhausted 400.
fn api(credits: bool) -> Upstream {
    Upstream::start(move |_, stream| {
        if credits {
            api_sse(stream, "from api");
        } else {
            reply_status(stream, 400, CREDITS_GONE);
        }
    })
}

struct Rig {
    fake: Fake,
    api: Upstream,
    agent: tempfile::TempDir,
    clock: Arc<AtomicI64>,
    router: Router,
}

/// A router over the fake `claude` (`vars`, subscription upstream `upstream`) and the fake API; `env` is the whole
/// process environment it sees.
fn make_rig(upstream: &str, vars: &[(&str, &str)], env: &[(&str, &str)], credits: bool) -> Rig {
    let fake = Fake::new();
    let api = api(credits);
    let agent = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicI64::new(NOW));
    let env: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let router = Router::new(RouterOptions {
        clock: {
            let clock = clock.clone();
            Arc::new(move || clock.load(Ordering::SeqCst))
        },
        env: Arc::new(move |name| env.get(name).cloned()),
        agent_dir: Some(agent.path().to_path_buf()),
        subscription: Some(fake.config(upstream, vars)),
        api_base_url: Some(api.url()),
    });
    Rig {
        fake,
        api,
        agent,
        clock,
        router,
    }
}

fn keyed() -> [(&'static str, &'static str); 1] {
    [("ANTHROPIC_API_KEY", KEY)]
}

fn transport() -> ReqwestTransport {
    ReqwestTransport::with_client(reqwest::Client::builder().no_proxy().build().unwrap(), None)
}

fn opus() -> Model {
    model("claude-opus-5-5")
}

fn run(rig: &Rig, context: &TranscriptContext) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
    run_as(rig, &opus(), context, &SimpleStreamOptions::default())
}

fn run_as(
    rig: &Rig,
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
    let events = Mutex::new(Vec::new());
    let mut sink = |event: &AssistantMessageEvent| events.lock().unwrap().push(event.clone());
    let transport = transport();
    let message = runtime().block_on(rig.router.stream(model, context, options, &transport, &mut sink));
    (message, events.into_inner().unwrap())
}

fn first_turn() -> TranscriptContext {
    context(vec![system("You are Ultron.", vec![rlm_tool()]), user("go")])
}

/// A conversation whose last assistant turn came from the subscription: signed thinking, text and an `rlm` call.
fn history() -> TranscriptContext {
    let assistant: AssistantMessage = serde_json::from_value(json!({
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "plan the cell", "thinkingSignature": "signed-by-anthropic"},
            {"type": "text", "text": "running it"},
            {"type": "toolCall", "id": "toolu_sub_1", "name": "rlm", "arguments": {"code": "print(1)"}},
        ],
        "api": "claude-subscription", "provider": "claude-subscription", "model": "claude-opus-5-5",
        "usage": ai::Usage::default(), "stopReason": "toolUse", "timestamp": 0,
    }))
    .unwrap();
    context(vec![
        system("You are Ultron.", vec![rlm_tool()]),
        user("go"),
        Message::Assistant(assistant),
        tool_result("toolu_sub_1", "1"),
    ])
}

fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn details(message: &AssistantMessage, kind: &str) -> Option<serde_json::Map<String, Value>> {
    diagnostic(message, kind).and_then(|d| d.details.clone())
}

fn kinds(events: &[AssistantMessageEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
            _ => "other",
        })
        .collect()
}

fn terminal_error(events: &[AssistantMessageEvent]) -> Option<String> {
    match events.last() {
        Some(AssistantMessageEvent::Error { error, .. }) => error.error_message.clone(),
        _ => None,
    }
}

#[test]
fn ai_claude_subscription_fallback_usage_limit_streams_the_same_model_and_context_from_the_api() {
    seal();
    let rig = make_rig(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", HIT_LIMIT)], &keyed(), true);
    let (message, events) = run(&rig, &history());

    // The answer streamed from the API: one start, the API's text, one done; nothing of the failed call.
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(text_of(&message), "from api");
    assert_eq!(
        (message.provider.as_str(), message.model.as_str()),
        ("anthropic", "claude-opus-5-5")
    );
    assert_eq!(
        kinds(&events),
        ["start", "text_start", "text_delta", "text_end", "done"]
    );
    let AssistantMessageEvent::Done { message: done, .. } = events.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(done, &message);

    // The same model id and context, with the credits key, went to the API.
    let requests = rig.api.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/messages?beta=true");
    assert_eq!(requests[0].header("x-api-key"), Some(KEY));
    assert_eq!(requests[0].header("authorization"), None);
    let body = requests[0].json();
    assert_eq!(body["model"], "claude-opus-5-5");
    // The loaded tool list is exactly [rlm]; the Anthropic provider's own `defer_loading` placeholder is never
    // loaded or callable (anthropic.rs `deferred_tool_placeholder`).
    let tools = body["tools"].as_array().unwrap();
    let loaded: Vec<&str> = tools
        .iter()
        .filter(|t| t["defer_loading"] != true)
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(loaded, ["rlm"]);
    assert!(
        tools
            .iter()
            .filter(|t| t["defer_loading"] == true)
            .all(|t| t["name"] == "__pi_deferred_placeholder__")
    );
    assert!(
        body["system"].to_string().contains("You are Ultron."),
        "{}",
        body["system"]
    );
    let messages = body["messages"].as_array().unwrap();
    // The conversation, then the provider's own effort marker for this model (anthropic.rs, mid-conversation effort).
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(messages[3]["role"], "system");
    assert_eq!(messages[0]["role"], "user");
    // Signed thinking from the subscription replays as text (the cross-provider transform), the call and its result
    // keep their id.
    let assistant = messages[1]["content"].as_array().unwrap();
    let types: Vec<&str> = assistant.iter().map(|b| b["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["text", "text", "tool_use"], "{assistant:?}");
    assert_eq!(assistant[0]["text"], "plan the cell");
    assert!(!messages[1].to_string().contains("signed-by-anthropic"));
    assert_eq!(assistant[2]["id"], "toolu_sub_1");
    assert_eq!(assistant[2]["name"], "rlm");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_sub_1");

    // One route notice; no reset was reported, so the hold is the default hour.
    let until = NOW + DEFAULT_HOLD_MS;
    let route = details(&message, ROUTE_DIAGNOSTIC).unwrap();
    assert_eq!(route["route"], "api");
    assert_eq!(route["until"], until);
    assert_eq!(route["model"], "claude-opus-5-5");
    assert_eq!(
        route["notice"],
        "Claude subscription limit reached; using API credits until 2026-09-21T15:13:20.000Z"
    );
    assert_eq!(
        route["reason"],
        "Native API error: You've hit your session limit · resets 3:45pm"
    );
    assert_eq!(rig.router.held_until(), Some(until));
    assert_eq!(rig.fake.records().len(), 1);
}

#[test]
fn ai_claude_subscription_fallback_transient_429_and_overload_do_not_fall_back() {
    seal();
    // A per-minute 429 from the upstream: no unified rejection header, no usage wording.
    let upstream = Upstream::start(|_, stream| {
        reply_status(
            stream,
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}"#,
        )
    });
    let throttled = make_rig(&upstream.url(), &[], &keyed(), true);
    let (message, events) = run(&throttled, &first_turn());
    let error = message.error_message.clone().unwrap();
    assert!(
        error.starts_with("Incomplete upstream response (first upstream attempt: status 429"),
        "{error}"
    );
    assert!(!error.contains(KEY_HINT));
    let status = details(&message, ERROR_DIAGNOSTIC).unwrap();
    assert_eq!(status["status"], 429);
    assert_eq!(status.get("usage_limit"), None);
    // The existing usage diagnostic of a 429 stays, as before.
    assert!(diagnostic(&message, "provider_usage_limit").is_some());
    assert_eq!(kinds(&events), ["start", "error"]);
    assert_eq!(terminal_error(&events), Some(error));
    assert!(throttled.api.requests().is_empty());
    assert_eq!(throttled.router.held_until(), None);

    for native in [
        "overloaded:API Error: Overloaded",
        "rate_limit:Request rate limit exceeded, slow down",
    ] {
        let rig = make_rig(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", native)], &keyed(), true);
        let (message, _) = run(&rig, &first_turn());
        assert_eq!(message.stop_reason, StopReason::Error, "{native}");
        assert!(rig.api.requests().is_empty(), "{native}");
        assert_eq!(rig.router.held_until(), None, "{native}");
        assert!(diagnostic(&message, ROUTE_DIAGNOSTIC).is_none(), "{native}");
    }
}

#[test]
fn ai_claude_subscription_fallback_error_after_streamed_content_does_not_fall_back() {
    seal();
    // The answer starts streaming, then the plan's limit cuts it off (native reports the rejection).
    let upstream = Upstream::start(|_, stream| {
        let start = json!({"type": "message_start", "message": {"id": "cut", "role": "assistant", "model": "claude-opus-5-5", "content": [], "usage": {"input_tokens": 1, "output_tokens": 0}}});
        let block = json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}});
        let delta = json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial answer"}});
        let _ = stream.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {start}\n\ndata: {block}\n\ndata: {delta}\n\n").as_bytes(),
        );
    });
    let limit = json!({"status": "rejected", "rateLimitType": "five_hour", "resetsAt": RESETS_AT}).to_string();
    let rig = make_rig(&upstream.url(), &[("FAKE_RATE_LIMIT", &limit)], &keyed(), true);
    let (message, events) = run(&rig, &first_turn());
    assert_eq!(message.stop_reason, StopReason::Error);
    // It was the plan's usage limit, but content had already reached the sink.
    let status = details(&message, ERROR_DIAGNOSTIC).unwrap();
    assert_eq!(status["usage_limit"], true);
    assert_eq!(status["resets_at"], RESETS_AT * 1000);
    assert!(rig.api.requests().is_empty());
    assert_eq!(rig.router.held_until(), None);
    let kinds = kinds(&events);
    assert_eq!(kinds.first(), Some(&"start"));
    assert!(kinds.contains(&"text_delta"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"error"));
    assert_eq!(terminal_error(&events), message.error_message);
    assert!(!message.error_message.unwrap().contains(KEY_HINT));
}

#[test]
fn ai_claude_subscription_fallback_without_an_api_key_returns_the_error_with_the_hint() {
    seal();
    // No ANTHROPIC_API_KEY; an Anthropic OAuth login in the store and an OAuth token in the env are the subscription
    // again, never a fallback.
    let env = [
        ("ANTHROPIC_OAUTH_TOKEN", "sk-ant-oat01-fixture"),
        ("ANTHROPIC_AUTH_TOKEN", "bearer-fixture"),
    ];
    let rig = make_rig(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", HIT_LIMIT)], &env, true);
    std::fs::write(
        rig.agent.path().join("auth.json"),
        json!({"anthropic": {"type": "oauth", "access": "sk-ant-oat01-stored", "refresh": "r", "expires": 1}})
            .to_string(),
    )
    .unwrap();
    let (message, events) = run(&rig, &first_turn());
    let expected = format!("Native API error: You've hit your session limit · resets 3:45pm ({KEY_HINT})");
    assert_eq!(message.error_message.as_deref(), Some(expected.as_str()));
    assert_eq!(kinds(&events), ["start", "error"]);
    assert_eq!(terminal_error(&events), Some(expected));
    assert!(rig.api.requests().is_empty());
    assert_eq!(rig.router.held_until(), None);
    assert!(diagnostic(&message, ROUTE_DIAGNOSTIC).is_none());

    // An OAuth-shaped ANTHROPIC_API_KEY is refused the same way.
    let oauth = make_rig(
        NO_UPSTREAM,
        &[("FAKE_NATIVE_ERROR", HIT_LIMIT)],
        &[("ANTHROPIC_API_KEY", "sk-ant-oat01-x")],
        true,
    );
    let (message, _) = run(&oauth, &first_turn());
    assert!(message.error_message.unwrap().ends_with(&format!("({KEY_HINT})")));
    assert!(oauth.api.requests().is_empty());
}

/// Without an explicit API URL the fallback goes to models.json's `anthropic` baseUrl, never past it to the public
/// API (a print-mode test once reached api.anthropic.com this way).
#[test]
fn ai_claude_subscription_fallback_goes_to_the_models_json_anthropic_base_url() {
    seal();
    let rig = make_rig(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", HIT_LIMIT)], &keyed(), true);
    std::fs::write(
        rig.agent.path().join("models.json"),
        json!({"providers": {"anthropic": {"baseUrl": rig.api.url()}}}).to_string(),
    )
    .unwrap();
    let env: HashMap<String, String> = keyed().iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let router = Router::new(RouterOptions {
        clock: Arc::new(|| NOW),
        env: Arc::new(move |name| env.get(name).cloned()),
        agent_dir: Some(rig.agent.path().to_path_buf()),
        subscription: Some(rig.fake.config(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", HIT_LIMIT)])),
        api_base_url: None,
    });
    let events = Mutex::new(Vec::new());
    let mut sink = |event: &AssistantMessageEvent| events.lock().unwrap().push(event.clone());
    let message = runtime().block_on(router.stream(
        &opus(),
        &first_turn(),
        &SimpleStreamOptions::default(),
        &transport(),
        &mut sink,
    ));
    assert_eq!(text_of(&message), "from api");
    assert_eq!(rig.api.requests().len(), 1);
}

#[test]
fn ai_claude_subscription_fallback_uses_the_auth_store_api_key() {
    seal();
    let rig = make_rig(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", HIT_LIMIT)], &[], true);
    std::fs::write(
        rig.agent.path().join("auth.json"),
        json!({"anthropic": {"type": "api_key", "key": "sk-ant-api03-stored"}}).to_string(),
    )
    .unwrap();
    let (message, _) = run(&rig, &first_turn());
    assert_eq!(text_of(&message), "from api");
    assert_eq!(rig.api.requests()[0].header("x-api-key"), Some("sk-ant-api03-stored"));
}

#[test]
fn ai_claude_subscription_fallback_holds_until_the_reset_then_tries_the_subscription_again() {
    seal();
    // The plan's window is exhausted on the first request only: the upstream rejects it with the unified headers.
    let reset = NOW / 1000 + 600;
    let answered = Arc::new(AtomicUsize::new(0));
    let upstream = {
        let answered = answered.clone();
        Upstream::start(move |_, stream| {
            if answered.fetch_add(1, Ordering::SeqCst) == 0 {
                let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your account's rate limit. Please try again later."}}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nanthropic-ratelimit-unified-status: rejected\r\nanthropic-ratelimit-unified-reset: {reset}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            } else {
                reply_sse(
                    stream,
                    &reply_events(
                        "msg_sub",
                        &[json!({"type": "text", "text": "from subscription"})],
                        "end_turn",
                        json!({"input_tokens": 1, "output_tokens": 1}),
                    ),
                );
            }
        })
    };
    let rig = make_rig(&upstream.url(), &[], &keyed(), true);

    let (first, _) = run(&rig, &first_turn());
    assert_eq!(text_of(&first), "from api");
    let until = reset * 1000;
    assert_eq!(rig.router.held_until(), Some(until));
    assert_eq!(details(&first, ROUTE_DIAGNOSTIC).unwrap()["until"], until);
    assert_eq!(rig.fake.records().len(), 1);

    // Until the reset no CLI runs; the turn goes straight to the API, without another notice.
    rig.clock.store(until - 1, Ordering::SeqCst);
    let (second, events) = run(&rig, &first_turn());
    assert_eq!(text_of(&second), "from api");
    assert_eq!(kinds(&events).first(), Some(&"start"));
    assert!(diagnostic(&second, ROUTE_DIAGNOSTIC).is_none());
    assert_eq!(rig.fake.records().len(), 1);
    assert_eq!(answered.load(Ordering::SeqCst), 1);
    assert_eq!(rig.api.requests().len(), 2);

    // At the reset the subscription is tried again, and the switch back is announced once.
    rig.clock.store(until, Ordering::SeqCst);
    let (third, _) = run(&rig, &first_turn());
    assert_eq!(text_of(&third), "from subscription");
    assert_eq!(third.provider, "claude-subscription");
    let route = details(&third, ROUTE_DIAGNOSTIC).unwrap();
    assert_eq!(route["route"], "subscription");
    assert_eq!(route["notice"], SUBSCRIPTION_NOTICE);
    assert_eq!(rig.router.held_until(), None);
    assert_eq!(rig.fake.records().len(), 2);
    assert_eq!(rig.api.requests().len(), 2);

    let (fourth, _) = run(&rig, &first_turn());
    assert_eq!(text_of(&fourth), "from subscription");
    assert!(diagnostic(&fourth, ROUTE_DIAGNOSTIC).is_none());
}

#[test]
fn ai_claude_subscription_fallback_api_credits_exhausted_names_both() {
    seal();
    // The recorded TypeScript fake's usage-limit run: a rejected rate_limit_event, then the CLI's text.
    let limit = json!({"status": "rejected", "rateLimitType": "five_hour", "resetsAt": RESETS_AT}).to_string();
    let text = format!("rate_limit:Claude AI usage limit reached|{RESETS_AT}");
    let rig = make_rig(
        NO_UPSTREAM,
        &[("FAKE_RATE_LIMIT", &limit), ("FAKE_NATIVE_ERROR", &text)],
        &keyed(),
        false,
    );
    let (message, events) = run(&rig, &first_turn());
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.clone().unwrap();
    assert_eq!(
        error,
        format!(
            "{BOTH_EXHAUSTED}: the subscription is out of usage until 2026-09-29T16:50:00.000Z, and the API answered: 400 {CREDITS_GONE}"
        )
    );
    assert_eq!(kinds(&events), ["error"]);
    assert_eq!(terminal_error(&events), Some(error));
    assert_eq!(details(&message, ROUTE_DIAGNOSTIC).unwrap()["until"], RESETS_AT * 1000);
    assert_eq!(rig.api.requests().len(), 1);
}

#[test]
fn ai_claude_subscription_fallback_setting_off_keeps_the_subscription_error() {
    seal();
    let rig = make_rig(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", HIT_LIMIT)], &keyed(), true);
    std::fs::write(
        rig.agent.path().join("settings.json"),
        json!({"claude": {"apiFallback": false}}).to_string(),
    )
    .unwrap();
    let (message, events) = run(&rig, &first_turn());
    assert_eq!(
        message.error_message.as_deref(),
        Some("Native API error: You've hit your session limit · resets 3:45pm")
    );
    assert_eq!(details(&message, ERROR_DIAGNOSTIC).unwrap()["usage_limit"], true);
    assert_eq!(kinds(&events), ["start", "error"]);
    assert!(rig.api.requests().is_empty());
    assert_eq!(rig.router.held_until(), None);

    // Turned back on, the same failure falls back.
    std::fs::write(
        rig.agent.path().join("settings.json"),
        json!({"claude": {"apiFallback": true}}).to_string(),
    )
    .unwrap();
    let (message, _) = run(&rig, &first_turn());
    assert_eq!(text_of(&message), "from api");
}

#[test]
fn ai_claude_subscription_fallback_classifies_only_the_plans_usage_limit() {
    seal();
    let api = |message: &str, status: Option<u16>| ClaudeError::Api {
        message: message.into(),
        status,
    };
    let none = Signals::default();
    let rate_limit = Signals {
        native_code: Some("rate_limit".into()),
        ..Signals::default()
    };
    // Recorded texts with a 429.
    assert_eq!(
        usage_limit(
            &api("Native API error: Claude AI usage limit reached|1790700600", Some(429)),
            &rate_limit
        ),
        Some(Some(1_790_700_600_000))
    );
    assert_eq!(
        usage_limit(
            &api(
                "Native API error: You've hit your session limit · resets 3:45pm",
                Some(429)
            ),
            &rate_limit
        ),
        Some(None)
    );
    assert_eq!(
        usage_limit(&api("5-hour limit reached ∙ resets 3pm", Some(429)), &none),
        Some(None)
    );
    assert_eq!(
        usage_limit(&api("You're out of extra usage", Some(429)), &none),
        Some(None)
    );
    // Transient 429s, overloads, and usage wording without a 429.
    assert_eq!(
        usage_limit(
            &api(
                "Number of request tokens has exceeded your per-minute rate limit",
                Some(429)
            ),
            &none
        ),
        None
    );
    assert_eq!(usage_limit(&api("API Error: Overloaded", Some(529)), &none), None);
    assert_eq!(
        usage_limit(&api("Claude AI usage limit reached|1790700600", Some(400)), &none),
        None
    );
    assert_eq!(usage_limit(&api("usage limit reached", None), &none), None);
    // A rejected rate_limit_event or unified header decides on its own; its reset wins over the text's.
    let event = Signals {
        event: Some(json!({"status": "rejected", "rateLimitType": "seven_day", "resetsAt": 1_790_000_000_123i64})),
        ..Signals::default()
    };
    assert_eq!(
        usage_limit(&api("Claude AI usage limit reached|1790700600", None), &event),
        Some(Some(1_790_000_000_123))
    );
    let allowed = Signals {
        event: Some(json!({"status": "allowed_warning", "resetsAt": 1_790_000_000})),
        ..Signals::default()
    };
    assert_eq!(usage_limit(&api("API Error: Connection error.", None), &allowed), None);
    let header = Signals {
        header_status: Some("rejected".into()),
        header_reset: Some("1790000600".into()),
        ..Signals::default()
    };
    assert_eq!(
        usage_limit(&api("rate limited", Some(429)), &header),
        Some(Some(1_790_000_600_000))
    );
    // Only API answers count: a refused request or a logged-out CLI never routes to the API.
    assert_eq!(
        usage_limit(&ClaudeError::Invalid("usage limit reached".into()), &header),
        None
    );
    assert_eq!(
        usage_limit(&ClaudeError::LoggedOut("usage limit reached".into()), &header),
        None
    );
}

#[test]
fn ai_claude_subscription_fallback_api_model_keeps_the_model_id() {
    seal();
    for (id, expected) in [
        ("claude-opus-5-5", "claude-opus-5-5"),
        ("opus", "claude-opus-5-5"),
        ("claude-opus-5-5[1m]", "claude-opus-5-5"),
        ("sonnet", "claude-sonnet-5-5"),
    ] {
        let api = fallback::api_model(&model(id));
        assert_eq!(
            (api.provider.as_str(), api.api.as_str()),
            ("anthropic", "anthropic-messages"),
            "{id}"
        );
        assert_eq!(api.id, expected);
        assert_eq!(api.base_url, "https://api.anthropic.com");
    }
}

#[test]
fn ai_claude_subscription_fallback_env_key_wins_and_never_reaches_the_cli() {
    seal();
    // One environment for both: the CLI runs on the plan without the key, the fallback uses it ahead of auth.json.
    let rig = make_rig(
        NO_UPSTREAM,
        &[("FAKE_NATIVE_ERROR", HIT_LIMIT), ("ANTHROPIC_API_KEY", KEY)],
        &keyed(),
        true,
    );
    std::fs::write(
        rig.agent.path().join("auth.json"),
        json!({"anthropic": {"type": "api_key", "key": "sk-ant-api03-stored"}}).to_string(),
    )
    .unwrap();
    let (message, _) = run(&rig, &first_turn());
    assert_eq!(text_of(&message), "from api");
    assert_eq!(rig.api.requests()[0].header("x-api-key"), Some(KEY));
    let child = rig.fake.last()["env"].clone();
    assert!(child.get("ANTHROPIC_API_KEY").is_none(), "the CLI saw the API key");
    assert!(!child.to_string().contains(KEY));
}

#[test]
fn ai_claude_subscription_fallback_model_missing_from_the_api_catalog_thinks_adaptively() {
    seal();
    // `claude-sonnet-5-5` is in the Anthropic catalog: the fallback takes its entry (price and limits from
    // https://platform.claude.com/docs/en/models/sonnet-5-5/overview).
    let sonnet = model("sonnet");
    let api = fallback::api_model(&sonnet);
    assert_eq!(api.id, "claude-sonnet-5-5");
    assert!(api.reasoning);
    assert_eq!(api.compat.as_ref().unwrap()["forceAdaptiveThinking"], true);
    assert_eq!(api.thinking_level_map.as_ref().unwrap()["off"], None);
    assert_eq!(
        (
            api.cost.input,
            api.cost.output,
            api.cost.cache_read,
            api.cost.cache_write
        ),
        (2.0, 10.0, 0.1, 2.5)
    );
    assert_eq!((api.context_window, api.max_tokens), (1_000_000, 128_000));
    // A model of a known family the catalog does not list yet takes the family's entry as its template.
    let next = fallback::api_model(&model("claude-sonnet-5-9"));
    let family = ai::models::builtin_model("anthropic", "claude-sonnet-5").unwrap();
    assert_eq!(next.id, "claude-sonnet-5-9");
    assert_eq!(next.cost, family.cost);
    assert_eq!(next.max_tokens, family.max_tokens);
    assert_eq!(next.compat.as_ref().unwrap()["forceAdaptiveThinking"], true);
    // An adaptive family with no catalog entry at all still thinks adaptively on the API.
    assert!(ai::models::builtin_models("anthropic").all(|m| !m.id.contains("mythos")));
    let mythos = fallback::api_model(&model("claude-mythos-5-1"));
    assert_eq!(
        (mythos.provider.as_str(), mythos.api.as_str()),
        ("anthropic", "anthropic-messages")
    );
    assert_eq!(mythos.compat.as_ref().unwrap()["forceAdaptiveThinking"], true);
    assert!(mythos.reasoning);

    let rig = make_rig(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", HIT_LIMIT)], &keyed(), true);
    let (off, _) = run_as(&rig, &sonnet, &first_turn(), &SimpleStreamOptions::default());
    assert_eq!(text_of(&off), "from api", "{:?}", off.error_message);
    assert_eq!(off.model, "claude-sonnet-5-5");
    let thinking = SimpleStreamOptions {
        reasoning: Some("high".into()),
        ..SimpleStreamOptions::default()
    };
    let (high, _) = run_as(&rig, &sonnet, &first_turn(), &thinking);
    assert_eq!(text_of(&high), "from api", "{:?}", high.error_message);
    let requests = rig.api.requests();
    assert_eq!(requests.len(), 2);
    let (off, high) = (requests[0].json(), requests[1].json());
    assert_eq!(off["model"], "claude-sonnet-5-5");
    // Sonnet 5.5 rejects `thinking: disabled`: its catalog entry maps `off` to unsupported, so a request without a
    // level thinks adaptively, as Opus 5.5's does.
    assert_eq!(off["thinking"]["type"], "adaptive", "{off}");
    assert_eq!(high["thinking"]["type"], "adaptive", "{high}");
}

#[test]
fn ai_claude_subscription_sealed_tests_cannot_reach_a_real_claude() {
    seal();
    // The production path with no injected config resolves `claude` from the process environment: in the sealed
    // test environment that is the sentinel, which exits at once without answering.
    let events = Mutex::new(Vec::new());
    let mut sink = |event: &AssistantMessageEvent| events.lock().unwrap().push(event.clone());
    let message = runtime().block_on(ai::claude_subscription::stream_simple(
        &opus(),
        &first_turn(),
        &SimpleStreamOptions::default(),
        &mut sink,
    ));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message.content.is_empty());
    let env: ai::claude_subscription::Env = std::env::vars().collect();
    let resolved = ai::claude_subscription::process::resolve_claude(None, &env).unwrap();
    assert!(resolved[0].ends_with("/bin/claude") && resolved[0].starts_with(&std::env::var(isolate::SEALED).unwrap()));
}
