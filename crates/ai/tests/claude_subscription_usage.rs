//! Claude Code's subscription usage on claude-subscription responses: `rateLimitsFromEvent` and `usageDiagnostic`
//! replayed from fixtures/claude-usage.json (recorded from the TypeScript root runner by
//! fixtures/generate-claude-usage.mjs), and the `claude_code_usage` diagnostic a call through the fake `claude`
//! carries when native reported a `rate_limit_event`, on success and on failure.

mod claude_subscription_support;

use ai::SimpleStreamOptions;
use ai::claude_subscription::usage::{
    CLAUDE_USAGE_DIAGNOSTIC, ClaudeRateLimits, rate_limits_from_event, usage_diagnostic,
};
use ai::types::{StopReason, TranscriptContext};
use claude_subscription_support::*;
use serde_json::{Map, Value, json};

const NO_UPSTREAM: &str = "http://127.0.0.1:9";

/// `ClaudeRateLimits` as the TypeScript object.
fn limits_json(limits: &ClaudeRateLimits) -> Value {
    let mut out = Map::new();
    if let Some(status) = &limits.status {
        out.insert("status".into(), json!(status));
    }
    let mut windows = Map::new();
    for (name, utilization, resets_at) in &limits.windows {
        let mut window = Map::new();
        if let Some(u) = utilization {
            window.insert("utilization".into(), u.clone());
        }
        if let Some(r) = resets_at {
            window.insert("resetsAt".into(), r.clone());
        }
        windows.insert(name.clone(), Value::Object(window));
    }
    out.insert("windows".into(), Value::Object(windows));
    Value::Object(out)
}

#[test]
fn ai_claude_subscription_usage_reads_rate_limit_events_like_the_ts_root_runner() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/claude-usage.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let limits = rate_limits_from_event(&case["event"]);
        assert_eq!(
            limits.as_ref().map(limits_json).unwrap_or(Value::Null),
            case["limits"],
            "{}",
            case["event"]
        );
        let diagnostic = limits.map(|l| serde_json::to_value(usage_diagnostic(&l, 0)).unwrap());
        assert_eq!(
            diagnostic.unwrap_or(Value::Null),
            case["diagnostic"],
            "{}",
            case["event"]
        );
    }
}

fn first_turn() -> TranscriptContext {
    context(vec![system("You are Ultron.", vec![rlm_tool()]), user("go")])
}

#[test]
fn ai_claude_subscription_usage_diagnostic_rides_on_the_call_that_saw_the_event() {
    seal();
    let fake = Fake::new();
    let limit = r#"{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.09},"seven_day":{"utilization":0.62,"resetsAt":1790700600}}}"#;
    let blocks = r#"[{"type":"text","text":"done"}]"#;
    let options = SimpleStreamOptions::default();
    let (message, _) = call(
        fake.config(
            NO_UPSTREAM,
            &[("FAKE_OFFLINE_BLOCKS", blocks), ("FAKE_RATE_LIMIT", limit)],
        ),
        &model("sonnet"),
        &first_turn(),
        &options,
    );
    assert_eq!(message.stop_reason, StopReason::Stop);
    let kinds: Vec<&str> = message.diagnostics.iter().flatten().map(|d| d.kind.as_str()).collect();
    assert_eq!(kinds, ["claude_subscription_usage", CLAUDE_USAGE_DIAGNOSTIC]);
    assert_eq!(
        Value::Object(
            diagnostic(&message, CLAUDE_USAGE_DIAGNOSTIC)
                .unwrap()
                .details
                .clone()
                .unwrap()
        ),
        json!({"status": "allowed", "windows": {"five_hour": {"utilization": 0.09}, "seven_day": {"utilization": 0.62, "resetsAt": 1790700600}}})
    );

    // A call without an event carries none.
    let (quiet, _) = call(
        fake.config(NO_UPSTREAM, &[("FAKE_OFFLINE_BLOCKS", blocks)]),
        &model("sonnet"),
        &first_turn(),
        &options,
    );
    assert!(diagnostic(&quiet, CLAUDE_USAGE_DIAGNOSTIC).is_none());

    // A failed call keeps it, between the error and the usage-limit diagnostics (the TypeScript order).
    let rejected = r#"{"status":"rejected","rateLimitType":"five_hour","utilization":1,"resetsAt":1790700600}"#;
    let (failed, _) = call(
        fake.config(
            NO_UPSTREAM,
            &[
                ("FAKE_RATE_LIMIT", rejected),
                (
                    "FAKE_NATIVE_ERROR",
                    "rate_limit:You've hit your session limit · resets 3:45pm",
                ),
            ],
        ),
        &model("sonnet"),
        &first_turn(),
        &options,
    );
    assert_eq!(failed.stop_reason, StopReason::Error);
    let kinds: Vec<&str> = failed.diagnostics.iter().flatten().map(|d| d.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "claude_subscription_error",
            CLAUDE_USAGE_DIAGNOSTIC,
            "provider_usage_limit"
        ]
    );
    assert_eq!(
        Value::Object(
            diagnostic(&failed, CLAUDE_USAGE_DIAGNOSTIC)
                .unwrap()
                .details
                .clone()
                .unwrap()
        ),
        json!({"status": "rejected", "windows": {"five_hour": {"utilization": 1, "resetsAt": 1790700600}}})
    );
}
